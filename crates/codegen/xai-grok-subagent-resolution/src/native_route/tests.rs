use super::*;
use crate::native_route::resolve::SyntheticCatalogEntry;
use crate::native_route::types::{
    AttemptLifecycleFact, CAP_MODEL_FAMILY_METADATA, CAP_ORDERED_CANDIDATES,
    CAP_REPLAY_SAFE_FALLBACK, CAP_ROUTE_RECEIPT, CapabilityRequirements, CapabilityState,
    NativeModelSelection, NativeRouteError, NativeSubagentRouteRequest, RECEIPT_SCHEMA,
    RejectedCandidate, RejectionCode, ResumePin, SCHEMA_VERSION, WorkerRoute,
};
use sha2::{Digest, Sha256};
use xai_grok_agent::config::ModelOverride;

fn entry(
    catalog_id: &str,
    wire: &str,
    route_key: &str,
    access: &str,
    ready: bool,
) -> SyntheticCatalogEntry {
    SyntheticCatalogEntry {
        catalog_id: catalog_id.into(),
        wire_model: wire.into(),
        route_key: route_key.into(),
        access_profile: access.into(),
        ready,
        unknown_readiness: false,
        local_only: false,
        harness: Some("grok".into()),
        context_tokens: Some(128_000),
        structured_output: true,
        named_capabilities: vec!["structured_output".into()],
    }
}

fn catalog() -> SyntheticCatalog {
    SyntheticCatalog {
        entries: vec![
            entry(
                "review-primary",
                "gpt-family-wire",
                "route-sub",
                "subscription",
                true,
            ),
            entry(
                "review-fallback",
                "gpt-family-wire",
                "route-payg",
                "payg",
                true,
            ),
            {
                let mut cold = entry(
                    "review-cold",
                    "other-wire",
                    "route-cold",
                    "subscription",
                    false,
                );
                cold.unknown_readiness = true;
                cold
            },
            {
                let mut unready = entry(
                    "review-unready",
                    "other-wire",
                    "route-unready",
                    "subscription",
                    false,
                );
                unready.unknown_readiness = false;
                unready
            },
            {
                let mut local = entry("review-local", "local-wire", "route-local", "local", true);
                local.local_only = true;
                local.structured_output = false;
                local.context_tokens = Some(8_000);
                local
            },
        ],
    }
}

fn request(selection: NativeModelSelection) -> NativeSubagentRouteRequest {
    NativeSubagentRouteRequest {
        schema_version: SCHEMA_VERSION,
        selection,
        required_capabilities: CapabilityRequirements::default(),
        capability_ceiling: None,
        consumer_policy_id: Some("verifier.default".into()),
        consumer_policy_digest: Some("digest-example".into()),
        parent_catalog_id: Some("review-primary".into()),
        parent_session_id: Some("parent-1".into()),
        child_session_id: Some("child-1".into()),
        resume: None,
    }
}

#[test]
fn discovery_marks_implemented_caps_supported_without_inference() {
    let caps = discover_capabilities();
    let by_id: std::collections::BTreeMap<_, _> = caps
        .iter()
        .map(|row| (row.capability_id.as_str(), row.state))
        .collect();
    assert_eq!(by_id[CAP_ORDERED_CANDIDATES], CapabilityState::Supported);
    assert_eq!(by_id[CAP_ROUTE_RECEIPT], CapabilityState::Supported);
    assert_eq!(
        by_id[CAP_MODEL_FAMILY_METADATA],
        CapabilityState::Unsupported
    );
    assert_eq!(
        by_id[CAP_REPLAY_SAFE_FALLBACK],
        CapabilityState::Unsupported
    );
}

#[test]
fn incompatible_schema_is_rejected() {
    let mut req = request(NativeModelSelection::Inherit);
    req.schema_version = 99;
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::IncompatibleSchema);
}

#[test]
fn exact_missing_never_uses_parent() {
    let req = request(NativeModelSelection::Exact {
        catalog_id: "does-not-exist".into(),
    });
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::ExactModelMissing);
    assert!(err.to_string().contains("refusing parent fallback"));
}

#[test]
fn exact_unready_never_uses_parent() {
    let req = request(NativeModelSelection::Exact {
        catalog_id: "review-unready".into(),
    });
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::RouteUnready);
}

#[test]
fn inherit_uses_explicit_parent() {
    let result =
        resolve_native_route(&request(NativeModelSelection::Inherit), &catalog(), 10, 1).unwrap();
    assert_eq!(result.selected_catalog_id, "review-primary");
    assert_eq!(result.receipt.selection_provenance, "inherit");
}

#[test]
fn ordered_candidates_preserve_declaration_order() {
    let req = request(NativeModelSelection::OrderedCandidates {
        catalog_ids: vec![
            "review-unready".into(),
            "review-fallback".into(),
            "review-primary".into(),
        ],
    });
    let result = resolve_native_route(&req, &catalog(), 11, 1).unwrap();
    assert_eq!(result.selected_catalog_id, "review-fallback");
    assert_eq!(result.rejected_candidates[0].catalog_id, "review-unready");
    assert_eq!(result.receipt.requested_catalog_ids[0], "review-unready");
}

#[test]
fn ordered_all_missing_preserves_exact_model_missing() {
    let req = request(NativeModelSelection::OrderedCandidates {
        catalog_ids: vec!["missing-a".into(), "missing-b".into()],
    });
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::ExactModelMissing);
}

#[test]
fn duplicate_wire_slug_stays_distinct_by_route_key() {
    let cat = catalog();
    let a = cat.get("review-primary").unwrap();
    let b = cat.get("review-fallback").unwrap();
    assert_eq!(a.wire_model, b.wire_model);
    assert_ne!(a.route_key, b.route_key);
    assert_ne!(a.access_profile, b.access_profile);
    let req = request(NativeModelSelection::Exact {
        catalog_id: "review-fallback".into(),
    });
    let result = resolve_native_route(&req, &cat, 12, 1).unwrap();
    assert_eq!(result.route_key, "route-payg");
    assert_eq!(result.receipt.access_profile, "payg");
}

#[test]
fn unknown_readiness_is_not_eligible() {
    let req = request(NativeModelSelection::Exact {
        catalog_id: "review-cold".into(),
    });
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::UnknownReadiness);
}

#[test]
fn local_only_rejects_cloud() {
    let mut req = request(NativeModelSelection::OrderedCandidates {
        catalog_ids: vec!["review-primary".into(), "review-local".into()],
    });
    req.required_capabilities.local_only = true;
    let result = resolve_native_route(&req, &catalog(), 13, 1).unwrap();
    assert_eq!(result.selected_catalog_id, "review-local");
    assert_eq!(
        result.rejected_candidates[0].reason_code,
        RejectionCode::LocalOnlyViolation
    );
}

#[test]
fn unknown_named_capability_is_not_eligible() {
    let mut req = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    req.required_capabilities
        .required_named_capabilities
        .push("unknown".into());
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::CapabilityUnknown);
}

#[test]
fn empty_candidates_are_invalid_not_inherit() {
    let req = request(NativeModelSelection::OrderedCandidates {
        catalog_ids: vec![],
    });
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::EmptyCandidates);
}

#[test]
fn declarative_empty_models_is_invalid_not_inherit() {
    let spec = DeclarativeNativeRouteSpec {
        models: Some(vec![]),
        ..DeclarativeNativeRouteSpec::default()
    };
    let err = parse_declarative_spec(spec).unwrap_err();
    assert_eq!(err.code(), RejectionCode::EmptyCandidates);

    let from_json: DeclarativeNativeRouteSpec = serde_json::from_str(r#"{"models":[]}"#).unwrap();
    let err = parse_declarative_spec(from_json).unwrap_err();
    assert_eq!(err.code(), RejectionCode::EmptyCandidates);

    let omitted = parse_declarative_spec(DeclarativeNativeRouteSpec::default()).unwrap();
    assert!(matches!(omitted.selection, NativeModelSelection::Inherit));
}

#[test]
fn declarative_conflict_model_and_models_is_rejected() {
    let spec = DeclarativeNativeRouteSpec {
        model: Some("review-primary".into()),
        models: Some(vec!["review-fallback".into()]),
        ..DeclarativeNativeRouteSpec::default()
    };
    let err = parse_declarative_spec(spec).unwrap_err();
    assert_eq!(err.code(), RejectionCode::ConflictingSyntax);
}

#[test]
fn declarative_models_become_ordered_candidates() {
    let spec = DeclarativeNativeRouteSpec {
        model: Some("inherit".into()),
        models: Some(vec!["review-primary".into(), "review-fallback".into()]),
        ..DeclarativeNativeRouteSpec::default()
    };
    let req = parse_declarative_spec(spec).unwrap();
    match req.selection {
        NativeModelSelection::OrderedCandidates { catalog_ids } => {
            assert_eq!(catalog_ids, vec!["review-primary", "review-fallback"]);
        }
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn resume_pins_source_route_and_refuses_rebind() {
    let mut req = request(NativeModelSelection::OrderedCandidates {
        catalog_ids: vec!["review-fallback".into()],
    });
    req.resume = Some(ResumePin {
        source_catalog_id: "review-primary".into(),
        source_receipt_digest: Some("abc".into()),
        source_route_key: Some("route-sub".into()),
    });
    let result = resolve_native_route(&req, &catalog(), 14, 2).unwrap();
    assert_eq!(result.selected_catalog_id, "review-primary");
    assert_eq!(result.receipt.selection_provenance, "resume");
    assert_eq!(result.receipt.resume_source_receipt.as_deref(), Some("abc"));

    req.resume = Some(ResumePin {
        source_catalog_id: "review-primary".into(),
        source_receipt_digest: Some("abc".into()),
        source_route_key: Some("route-payg".into()),
    });
    let err = resolve_native_route(&req, &catalog(), 14, 2).unwrap_err();
    assert_eq!(err.code(), RejectionCode::ResumeRoutePinned);
}

#[test]
fn resume_without_route_key_is_rejected() {
    let mut req = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    req.resume = Some(ResumePin {
        source_catalog_id: "review-primary".into(),
        source_receipt_digest: Some("abc".into()),
        source_route_key: None,
    });
    let err = resolve_native_route(&req, &catalog(), 14, 2).unwrap_err();
    assert_eq!(err.code(), RejectionCode::ResumeRoutePinned);
}

#[test]
fn oauth_label_is_not_secret_material() {
    let mut req = request(NativeModelSelection::Inherit);
    req.consumer_policy_id = Some("oauth-review".into());
    resolve_native_route(&req, &catalog(), 1, 1).unwrap();
}

#[test]
fn receipt_digest_is_deterministic_and_secret_free() {
    let req = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    let a = resolve_native_route(&req, &catalog(), 20, 1).unwrap();
    let b = resolve_native_route(&req, &catalog(), 99, 1).unwrap();
    assert_eq!(a.receipt.route_digest, b.receipt.route_digest);
    assert_eq!(a.receipt.route_digest.len(), 64);
    let blob = serde_json::to_string(&a.receipt)
        .unwrap()
        .to_ascii_lowercase();
    for needle in ["sk-", "bearer ", "acct_", "api_key", "authorization"] {
        assert!(!blob.contains(needle), "receipt leaked {needle}");
    }
}

#[test]
fn request_secret_material_is_rejected() {
    let mut req = request(NativeModelSelection::Inherit);
    req.consumer_policy_id = Some("sk-secret-example".into());
    let err = resolve_native_route(&req, &catalog(), 1, 1).unwrap_err();
    assert!(matches!(err, NativeRouteError::Rejected(_, _)));

    let mut ceiling = request(NativeModelSelection::Inherit);
    ceiling.capability_ceiling = Some("Bearer token-example".into());
    let err = resolve_native_route(&ceiling, &catalog(), 1, 1).unwrap_err();
    assert!(matches!(err, NativeRouteError::Rejected(_, _)));

    let mut resume = request(NativeModelSelection::Inherit);
    resume.resume = Some(ResumePin {
        source_catalog_id: "review-primary".into(),
        source_receipt_digest: Some("Bearer token-example".into()),
        source_route_key: None,
    });
    let err = resolve_native_route(&resume, &catalog(), 1, 1).unwrap_err();
    assert!(matches!(err, NativeRouteError::Rejected(_, _)));
}

#[test]
fn receipt_digest_binds_harness_ceiling_and_resume() {
    let mut req = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    let baseline = resolve_native_route(&req, &catalog(), 20, 1).unwrap();
    req.capability_ceiling = Some("read-only".into());
    let with_ceiling = resolve_native_route(&req, &catalog(), 20, 1).unwrap();
    assert_ne!(
        baseline.receipt.route_digest,
        with_ceiling.receipt.route_digest
    );

    let mut cat = catalog();
    cat.entries[0].harness = Some("other-harness".into());
    let with_harness = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &cat,
        20,
        1,
    )
    .unwrap();
    assert_ne!(
        baseline.receipt.route_digest,
        with_harness.receipt.route_digest
    );

    let mut resume = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    resume.resume = Some(ResumePin {
        source_catalog_id: "review-primary".into(),
        source_receipt_digest: Some("abc".into()),
        source_route_key: Some("route-sub".into()),
    });
    let with_resume = resolve_native_route(&resume, &catalog(), 20, 2).unwrap();
    assert_ne!(
        baseline.receipt.route_digest,
        with_resume.receipt.route_digest
    );

    let mut with_caps = request(NativeModelSelection::Exact {
        catalog_id: "review-primary".into(),
    });
    with_caps.required_capabilities.structured_output = true;
    let bound_caps = resolve_native_route(&with_caps, &catalog(), 20, 1).unwrap();
    assert_ne!(
        baseline.receipt.route_digest,
        bound_caps.receipt.route_digest
    );
}

#[test]
fn inspect_document_matches_capability_registry() {
    let doc = inspect_document(Vec::new()).expect("empty inspect");
    assert_eq!(doc.schema, "medley.native-subagent-route.inspect/v1");
    assert_eq!(doc.host, "medley");
    assert_eq!(doc.capabilities.len(), 5);
    let json = serde_json::to_value(&doc).unwrap();
    assert_eq!(json["schema"], "medley.native-subagent-route.inspect/v1");
}

#[test]
fn snake_case_requirement_field_is_rejected() {
    let err = serde_json::from_str::<CapabilityRequirements>(r#"{"local_only":true}"#);
    assert!(err.is_err());
    let spec = serde_json::from_str::<DeclarativeNativeRouteSpec>(
        r#"{"routingRequirements":{"local_only":true}}"#,
    );
    assert!(spec.is_err());
    let ok = serde_json::from_str::<CapabilityRequirements>(r#"{"localOnly":true}"#).unwrap();
    assert!(ok.local_only);
}

#[test]
fn unknown_declarative_route_field_is_rejected() {
    let err = serde_json::from_str::<DeclarativeNativeRouteSpec>(
        r#"{"model":"cloud","routingRequirement":{"localOnly":true}}"#,
    );
    assert!(err.is_err());
    let ok = serde_json::from_str::<DeclarativeNativeRouteSpec>(
        r#"{"model":"review-primary","routingRequirements":{"localOnly":true}}"#,
    )
    .unwrap();
    assert!(ok.routing_requirements.local_only);
}

#[test]
fn inspect_document_rejects_forged_receipt_digest() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        20,
        1,
    )
    .unwrap();
    let ok = inspect_document(vec![result.receipt.clone()]).expect("valid receipt");
    assert_eq!(ok.receipts.len(), 1);

    let mut forged = result.receipt;
    forged.route_digest = "0".repeat(64);
    let err = inspect_document(vec![forged]).unwrap_err();
    assert_eq!(err.code(), RejectionCode::UnsupportedContract);
}

#[test]
fn inspect_document_rejects_incompatible_receipt_schema_version() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        20,
        1,
    )
    .unwrap();
    let mut forged = result.receipt;
    forged.schema_version = 2;
    let blob = serde_json::to_vec(&forged.canonical_payload()).unwrap();
    forged.route_digest = format!("{:x}", Sha256::digest(blob));
    let err = inspect_document(vec![forged]).unwrap_err();
    assert_eq!(err.code(), RejectionCode::UnsupportedContract);
}

#[test]
fn inspect_document_rejects_unknown_selection_mode() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        20,
        1,
    )
    .unwrap();
    let mut forged = result.receipt;
    forged.selection_mode = "bogus".into();
    let blob = serde_json::to_vec(&forged.canonical_payload()).unwrap();
    forged.route_digest = format!("{:x}", Sha256::digest(blob));
    let err = inspect_document(vec![forged]).unwrap_err();
    assert_eq!(err.code(), RejectionCode::UnsupportedContract);
}

#[test]
fn unknown_request_field_is_rejected() {
    let err = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"inherit"},"requiredCapability":{"localOnly":true}}"#,
    );
    assert!(err.is_err());
    let ok = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"inherit"},"requiredCapabilities":{"localOnly":true}}"#,
    )
    .unwrap();
    assert!(ok.required_capabilities.local_only);
}

#[test]
fn unknown_selection_object_field_is_rejected() {
    let err = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"exact","catalog_id":"cloud","requiredCapabilities":{"localOnly":true}}}"#,
    );
    assert!(err.is_err());
    let ok = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"exact","catalog_id":"review-primary"},"requiredCapabilities":{"localOnly":true}}"#,
    )
    .unwrap();
    assert!(ok.required_capabilities.local_only);
    match ok.selection {
        NativeModelSelection::Exact { catalog_id } => {
            assert_eq!(catalog_id, "review-primary");
        }
        other => panic!("expected exact, got {other:?}"),
    }
}

#[test]
fn unknown_resume_pin_field_is_rejected() {
    let err = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"inherit"},"resume":{"sourceCatalogId":"review-primary","sourceRouteKe":"lane-a"}}"#,
    );
    assert!(err.is_err());
    let ok = serde_json::from_str::<NativeSubagentRouteRequest>(
        r#"{"schemaVersion":1,"selection":{"mode":"inherit"},"resume":{"sourceCatalogId":"review-primary","sourceRouteKey":"lane-a"}}"#,
    )
    .unwrap();
    assert_eq!(
        ok.resume.as_ref().unwrap().source_route_key.as_deref(),
        Some("lane-a")
    );
}

#[test]
fn external_executor_is_not_a_medley_provider_route() {
    let route = WorkerRoute::ExternalExecutor {
        descriptor: "codex --yolo".into(),
    };
    let err = resolve_worker_route(&route, &catalog(), 1, 1).unwrap_err();
    assert_eq!(err.code(), RejectionCode::UnsupportedContract);
}

#[test]
fn fallback_is_refused_after_output_or_tool() {
    let after_output = admit_cross_route_fallback(&[AttemptLifecycleFact::VisibleOutputCommitted]);
    assert!(!after_output.admitted);
    assert_eq!(
        after_output.reason_code,
        RejectionCode::FallbackReplayUnsafe
    );
    let after_tool = admit_cross_route_fallback(&[AttemptLifecycleFact::ToolCallEmitted]);
    assert!(!after_tool.admitted);
    let idle = admit_cross_route_fallback(&[AttemptLifecycleFact::AttemptStarted]);
    assert!(!idle.admitted);
}

#[test]
fn ux_snapshot_compact_row_keeps_status_without_color() {
    let snap = snapshot_from_model_override(
        "verifier",
        "verifier",
        "project",
        true,
        false,
        false,
        &ModelOverride::Inherit,
        Some("read-only"),
        1,
    );
    let row = format_compact_row(&snap, 80);
    assert!(row.contains("verifier"));
    assert!(row.contains("enabled"));
    assert!(row.contains("inherit"));
    assert!(row.contains("unknown"));
    assert!(!row.contains("ready"));
    assert!(row.contains("read-only"));
    assert_eq!(snap.route_status, RouteStatus::Unknown);
    assert!(!row.contains("\u{1b}"));
    let detail = format_route_detail(&snap);
    assert!(
        detail
            .iter()
            .any(|line| line.contains("Selection: inherit"))
    );
}

#[test]
fn ux_snapshot_from_resolution_carries_receipt_digest() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        1,
        3,
    )
    .unwrap();
    let snap = snapshot_from_resolution(
        "verifier",
        "verifier",
        "plugin",
        true,
        true,
        false,
        &result,
        Some("read-only"),
        7,
    );
    assert_eq!(snap.route_status, RouteStatus::Ready);
    assert_eq!(snap.attempt, Some(3));
    assert_eq!(
        snap.selected_catalog_id.as_deref(),
        Some(result.receipt.selected_catalog_id.as_str())
    );
    assert_eq!(
        snap.selected_wire_model.as_deref(),
        Some(result.receipt.selected_wire_model.as_str())
    );
    assert_eq!(
        snap.route_receipt_digest.as_deref(),
        Some(result.receipt.route_digest.as_str())
    );
}

#[test]
fn ux_snapshot_rejects_forged_receipt_digest() {
    let mut result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        1,
        3,
    )
    .unwrap();
    result.receipt.selected_catalog_id = "forged-catalog".into();
    let snap = snapshot_from_resolution(
        "verifier",
        "verifier",
        "plugin",
        true,
        true,
        false,
        &result,
        Some("read-only"),
        7,
    );
    assert_eq!(snap.route_status, RouteStatus::Incompatible);
    assert!(snap.selected_catalog_id.is_none());
    assert!(snap.route_receipt_digest.is_none());
}

#[test]
fn ux_snapshot_rejects_unknown_selection_mode() {
    let mut result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        1,
        3,
    )
    .unwrap();
    result.receipt.selection_mode = "bogus".into();
    let blob = serde_json::to_vec(&result.receipt.canonical_payload()).unwrap();
    result.receipt.route_digest = format!("{:x}", Sha256::digest(blob));
    let snap = snapshot_from_resolution(
        "verifier",
        "verifier",
        "plugin",
        true,
        true,
        false,
        &result,
        Some("read-only"),
        7,
    );
    assert_eq!(snap.route_status, RouteStatus::Incompatible);
    assert!(snap.selected_catalog_id.is_none());
    assert!(snap.route_receipt_digest.is_none());
}

#[test]
fn thousand_entry_format_stays_bounded() {
    let snap = snapshot_from_model_override(
        "agent",
        "agent-with-a-very-long-unicode-名稱",
        "project",
        true,
        false,
        true,
        &ModelOverride::Override("review-primary".into()),
        Some("read-write"),
        1,
    );
    for width in [20, 40, 80, 160] {
        for _ in 0..1_000 {
            let row = format_compact_row(&snap, width);
            assert!(unicode_width::UnicodeWidthStr::width(row.as_str()) <= width);
        }
    }
    let cjk = format_compact_row(&snap, 8);
    assert!(unicode_width::UnicodeWidthStr::width(cjk.as_str()) <= 8);
}

#[test]
fn configuration_only_exact_does_not_claim_selected_catalog() {
    let snap = snapshot_from_model_override(
        "verifier",
        "verifier",
        "project",
        true,
        false,
        false,
        &ModelOverride::Override("review-primary".into()),
        Some("read-only"),
        1,
    );
    assert_eq!(snap.route_status, RouteStatus::Unknown);
    assert_eq!(snap.selected_catalog_id, None);
    assert_eq!(snap.requested_model_refs, vec!["review-primary"]);
    let detail = format_route_detail(&snap);
    assert!(
        detail
            .iter()
            .any(|line| line.contains("Requested: review-primary"))
    );
    assert!(!detail.iter().any(|line| line.contains("Selected catalog:")));
    let row = format_compact_row(&snap, 80);
    assert!(row.contains("review-primary"));
    assert!(row.contains("unknown"));
}

#[test]
fn receipt_digest_binds_serialized_schema() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        1,
        1,
    )
    .unwrap();
    let mut receipt = result.receipt.clone();
    assert_eq!(
        receipt.canonical_payload().get("schema"),
        Some(&serde_json::Value::String(RECEIPT_SCHEMA.into()))
    );
    let original = receipt.route_digest.clone();
    receipt.schema = "attacker.schema".into();
    let blob = serde_json::to_vec(&receipt.canonical_payload()).unwrap();
    let mutated = format!("{:x}", Sha256::digest(&blob));
    assert_ne!(original, mutated);

    let mut with_reject = result.receipt.clone();
    with_reject.rejected_candidates.push(RejectedCandidate {
        catalog_id: "review-unready".into(),
        wire_model: None,
        route_key: None,
        reason_code: RejectionCode::RouteUnready,
        message: "catalog not ready".into(),
    });
    let with_message = format!(
        "{:x}",
        Sha256::digest(&serde_json::to_vec(&with_reject.canonical_payload()).unwrap())
    );
    with_reject.rejected_candidates[0].message = "forged explanation".into();
    let forged_message = format!(
        "{:x}",
        Sha256::digest(&serde_json::to_vec(&with_reject.canonical_payload()).unwrap())
    );
    assert_ne!(with_message, forged_message);
}

#[test]
fn agent_definition_parses_models_as_ordered_candidates() {
    let def = crate::AgentDefinition::parse(
        "---\nname: verifier\ndescription: dual review\nmodels:\n  - review-primary\n  - review-fallback\n---\n",
    )
    .unwrap();
    assert_eq!(
        def.models,
        vec!["review-primary".to_string(), "review-fallback".to_string()]
    );
    let req = request_from_agent_definition(&def, None, None, None, None).unwrap();
    assert_eq!(
        req.selection,
        NativeModelSelection::OrderedCandidates {
            catalog_ids: vec!["review-primary".into(), "review-fallback".into()],
        }
    );
}

#[test]
fn agent_definition_rejects_model_and_models_conflict() {
    let err = crate::AgentDefinition::parse(
        "---\nname: verifier\ndescription: dual review\nmodel: review-primary\nmodels:\n  - review-fallback\n---\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("model and models"));
}

#[test]
fn usage_facts_from_receipt_are_secret_free() {
    let result = resolve_native_route(
        &request(NativeModelSelection::Exact {
            catalog_id: "review-primary".into(),
        }),
        &catalog(),
        1,
        2,
    )
    .unwrap();
    let facts = usage_facts_from_receipt(&result.receipt);
    assert_eq!(facts["catalogId"], "review-primary");
    assert_eq!(facts["wireModel"], "gpt-family-wire");
    assert_eq!(facts["accessProfile"], "subscription");
    assert_eq!(facts["attempt"], 2);
    assert_eq!(facts["selectionMode"], "exact");
    assert_eq!(facts["routeDigest"], result.receipt.route_digest);
    let blob = facts.to_string().to_ascii_lowercase();
    assert!(!blob.contains("sk-"));
    assert!(!blob.contains("bearer"));
    assert!(!blob.contains("api_key"));
}

#[test]
fn inspect_document_includes_live_receipts() {
    let result =
        resolve_native_route(&request(NativeModelSelection::Inherit), &catalog(), 1, 1).unwrap();
    let doc = inspect_document(vec![result.receipt.clone()]);
    assert_eq!(doc.receipts.len(), 1);
    assert_eq!(doc.receipts[0].selected_catalog_id, "review-primary");
}

#[test]
fn snapshot_from_agent_definition_uses_models_list() {
    let mut def = crate::AgentDefinition::explore();
    def.models = vec!["review-primary".into(), "review-fallback".into()];
    let snap = snapshot_from_agent_definition(
        "verifier",
        "verifier",
        "project",
        true,
        false,
        false,
        &def,
        Some("read-only"),
        1,
    );
    assert_eq!(snap.selection_mode, AgentSelectionMode::OrderedCandidates);
    assert_eq!(
        snap.requested_model_refs,
        vec!["review-primary".to_string(), "review-fallback".to_string()]
    );
}
