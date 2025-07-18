# Copyright 2022 Pants project contributors (see CONTRIBUTORS.md).
# Licensed under the Apache License, Version 2.0 (see LICENSE).
from __future__ import annotations

import logging
from collections import defaultdict
from collections.abc import Sequence
from dataclasses import dataclass
from typing import TypeVar

from pants.bsp.protocol import BSPHandlerMapping
from pants.bsp.spec.resources import ResourcesItem, ResourcesParams, ResourcesResult
from pants.bsp.util_rules.targets import (
    BSPBuildTargetInternal,
    BSPResourcesRequest,
    BSPResourcesResult,
    resolve_bsp_build_target_addresses,
    resolve_bsp_build_target_identifier,
)
from pants.engine.fs import Workspace
from pants.engine.internals.native_engine import EMPTY_DIGEST, MergeDigests
from pants.engine.internals.selectors import Get, concurrently
from pants.engine.intrinsics import merge_digests
from pants.engine.rules import _uncacheable_rule, collect_rules, implicitly, rule
from pants.engine.target import FieldSet
from pants.engine.unions import UnionMembership, UnionRule

_logger = logging.getLogger(__name__)

_FS = TypeVar("_FS", bound=FieldSet)


class ResourcesRequestHandlerMapping(BSPHandlerMapping):
    method_name = "buildTarget/resources"
    request_type = ResourcesParams
    response_type = ResourcesResult


@dataclass(frozen=True)
class ResourcesForOneBSPTargetRequest:
    bsp_target: BSPBuildTargetInternal


@rule
async def resources_bsp_target(
    request: ResourcesForOneBSPTargetRequest,
    union_membership: UnionMembership,
) -> BSPResourcesResult:
    targets = await resolve_bsp_build_target_addresses(request.bsp_target, **implicitly())
    resources_request_types: Sequence[type[BSPResourcesRequest]] = union_membership.get(
        BSPResourcesRequest
    )
    field_sets_by_request_type: dict[type[BSPResourcesRequest], set[FieldSet]] = defaultdict(set)
    for target in targets:
        for resources_request_type in resources_request_types:
            field_set_type = resources_request_type.field_set_type
            if field_set_type.is_applicable(target):
                field_set = field_set_type.create(target)
                field_sets_by_request_type[resources_request_type].add(field_set)

    resources_results = await concurrently(
        Get(
            BSPResourcesResult,
            BSPResourcesRequest,
            resources_request_type(bsp_target=request.bsp_target, field_sets=tuple(field_sets)),
        )
        for resources_request_type, field_sets in field_sets_by_request_type.items()
    )

    resources = tuple(sorted({resource for rr in resources_results for resource in rr.resources}))

    output_digest = await merge_digests(
        MergeDigests([rr.output_digest for rr in resources_results])
    )

    return BSPResourcesResult(
        resources=resources,
        output_digest=output_digest,
    )


@_uncacheable_rule
async def bsp_resources_request(
    request: ResourcesParams,
    workspace: Workspace,
) -> ResourcesResult:
    bsp_targets = await concurrently(
        resolve_bsp_build_target_identifier(bsp_target_id, **implicitly())
        for bsp_target_id in request.targets
    )

    resources_results = await concurrently(
        resources_bsp_target(
            ResourcesForOneBSPTargetRequest(
                bsp_target=bsp_target,
            ),
            **implicitly(),
        )
        for bsp_target in bsp_targets
    )

    # TODO: Need to determine how resources are expected to be exposed. Directories? Individual files?
    # Initially, it looks like loose directories.
    output_digest = await merge_digests(MergeDigests([r.output_digest for r in resources_results]))
    if output_digest != EMPTY_DIGEST:
        workspace.write_digest(output_digest, path_prefix=".pants.d/bsp")

    return ResourcesResult(
        tuple(
            ResourcesItem(
                target,
                rr.resources,
            )
            for target, rr in zip(request.targets, resources_results)
        )
    )


def rules():
    return (
        *collect_rules(),
        UnionRule(BSPHandlerMapping, ResourcesRequestHandlerMapping),
    )
