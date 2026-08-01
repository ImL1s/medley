fn expose_generic_credential_fields(auth: &GrokAuth, snapshot: &CredentialSnapshot) {
    tracing::warn!(%auth.key, "raw Grok auth credential");
    tracing::warn!(%snapshot.token, "raw credential snapshot token");

    let auth_value = auth.key.clone();
    tracing::warn!(%auth_value, "aliased Grok auth credential");

    let snapshot_value = snapshot.token.clone();
    tracing::warn!(%snapshot_value, "aliased credential snapshot token");
}

fn expose_associated_factory_fields() {
    let auth = GrokAuth::new(obtain_auth_material());
    tracing::warn!(%auth.key, "factory-created Grok auth credential");

    let snapshot = CredentialSnapshot::new(obtain_snapshot_material());
    tracing::warn!(%snapshot.token, "factory-created credential snapshot token");

    let boxed_auth = Box::new(GrokAuth::new(obtain_auth_material()));
    tracing::warn!(%boxed_auth.key, "boxed Grok auth credential");

    let shared_snapshot = Arc::new(CredentialSnapshot {
        token: obtain_snapshot_material(),
    });
    tracing::warn!(%shared_snapshot.token, "shared credential snapshot token");

    let local_auth = Rc::new(GrokAuth {
        key: obtain_auth_material(),
    });
    let local_value = local_auth.key.clone();
    tracing::warn!(%local_value, "aliased Rc Grok auth credential");
}

fn expose_destructured_fields(auth: GrokAuth, snapshot: CredentialSnapshot) {
    let GrokAuth { key, .. } = auth;
    tracing::warn!(%key, "destructured Grok auth credential");

    let CredentialSnapshot { token, .. } = snapshot;
    tracing::warn!(%token, "destructured credential snapshot token");
}

fn expose_typed_closure_fields() {
    let auth_observer = |auth: &GrokAuth| tracing::warn!(%auth.key, "closure auth");
    let snapshot_observer =
        |snapshot: &CredentialSnapshot| tracing::warn!(%snapshot.token, "closure snapshot");
}

fn expose_match_destructured_fields(auth: GrokAuth, snapshot: CredentialSnapshot) {
    match auth {
        GrokAuth { key, .. } => tracing::warn!(%key, "match-destructured auth"),
    }
    match snapshot {
        CredentialSnapshot { token, .. } => {
            tracing::warn!(%token, "match-destructured snapshot")
        }
    }
}

fn expose_parameter_destructuring(GrokAuth { key, .. }: GrokAuth) {
    tracing::warn!(%key, "parameter-destructured auth");
}

fn expose_closure_parameter_destructuring() {
    let observer = |CredentialSnapshot { token, .. }: CredentialSnapshot| {
        tracing::warn!(%token, "closure-parameter-destructured snapshot")
    };
}

fn expose_option_extractions(snapshot: &CredentialSnapshot) {
    let unwrapped = snapshot.token.as_deref().unwrap();
    tracing::warn!(%unwrapped, "unwrapped snapshot token");

    let expected = snapshot.token.as_deref().expect("credential required");
    tracing::warn!(%expected, "expected snapshot token");

    let defaulted = snapshot.token.clone().unwrap_or_default();
    tracing::warn!(%defaulted, "defaulted snapshot token");

    let fallback = snapshot
        .token
        .clone()
        .unwrap_or_else(|| obtain_snapshot_material());
    tracing::warn!(%fallback, "fallback snapshot token");

    if let Some(token) = snapshot.token.as_deref() {
        tracing::warn!(%token, "if-let snapshot token");
        let copied = token;
        tracing::warn!(%copied, "if-let aliased snapshot token");
    }

    match snapshot.token.as_deref() {
        Some(matched) => tracing::warn!(%matched, "match-extracted snapshot token"),
        None => {}
    }

    let Some(let_else_token) = snapshot.token.as_deref() else {
        return;
    };
    tracing::warn!(%let_else_token, "let-else snapshot token");

    let Ok(ok_token) = snapshot.token.as_deref().ok_or(()) else {
        return;
    };
    tracing::warn!(%ok_token, "Result let-else snapshot token");

    match snapshot.token.as_deref() {
        Some(guarded_token) if ready() => {
            tracing::warn!(%guarded_token, "guarded match snapshot token")
        }
        _ => {}
    }
}

fn expose_question_mark_extraction(snapshot: &CredentialSnapshot) -> Option<()> {
    let question_token = snapshot.token.as_deref()?;
    tracing::warn!(%question_token, "question-mark snapshot token");
    Some(())
}
