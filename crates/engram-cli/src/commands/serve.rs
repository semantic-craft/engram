//! `engram serve` — MCP server with optional filesystem watcher.

use std::convert::Infallible;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{Method, Request, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use engram_consolidate::{
    AutoImproveReviewConfig, Consolidator, run_auto_improve_review, run_lint, run_sweep,
};
use engram_core::{
    ActiveProject, ActorContext, PagePath, ProjectId, Sanitizer, SessionId, WorkspaceId,
};
use engram_hooks::{DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT, HookState, ProjectCacheStore, hook_router};
use engram_llm::{Embedder, LlmProvider, ProviderHealth, build_embedder, build_provider};
use engram_mcp::{AdminState, EngramServer, admin_router};
use engram_store::{
    ApproveAutoImproveProposalResult, AutoImproveProposalOperation, EmbeddingWrite,
    NewAutoImproveProposal, ReaderPool, StageAutoImproveRun, Store, WriterHandle, f32_vec_to_bytes,
};
use engram_web;
use engram_wiki::{WatcherHandle, Wiki, migrations, run_wiki_migrations};
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tokio_util::sync::CancellationToken;
use tower::service_fn;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;
use tracing::info;

use crate::auth::{AuthState, require_bearer};
use crate::cli::{ServeArgs, TransportKind};
use crate::config::{AutoImproveSettings, Config, MaintenanceSettings};

/// 10 MB cap on inbound HTTP bodies. The /hook ingress accepts the
/// agent's raw payload which can include a tool output excerpt
/// (capped at 2 KB on our side via `truncate_excerpt`), but Claude
/// Code et al. send the full envelope, which can run to a few KB.
/// 10 MB is generous headroom; without a cap, axum streams unbounded
/// bodies into memory (audit critical #2).
const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;
const EMBEDDING_WRITE_BATCH: usize = 100;

/// `POST /admin/bootstrap` may carry a large JSON array of sources even
/// after client-side prune; keep hooks/MCP at [`MAX_BODY_BYTES`].
const BOOTSTRAP_MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

struct ConsolidatorSetup {
    server: EngramServer,
    consolidator: Option<Arc<Consolidator>>,
    admin_llm: Option<Arc<dyn LlmProvider>>,
}

/// Run the `serve` subcommand.
///
/// # Errors
/// Returns an error if the store cannot be opened, the watcher cannot
/// install, or the transport setup fails.
pub async fn run(config: &Config, args: ServeArgs) -> Result<()> {
    validate_web_ui_args(args.enable_web, args.web_ui_dir.as_deref())?;

    // Merge config + CLI CORS origins (config first, CLI adds new entries).
    // Validation runs before binding so a misconfigured origin is caught early.
    let cors_origins = merge_cors_origins(&config.cors_allow_origins, &args.cors_allow_origin);
    validate_cors_origins(&cors_origins)?;

    let store = Store::open(&config.data_dir)
        .with_context(|| format!("opening store at {}", config.data_dir.display()))?;

    // One-shot legacy heal (issue #103): NULL out any project repo_path that
    // is a prefix-match catch-all. That means the $HOME and filesystem-root
    // sentinels, plus any path that exists locally but is not a git work-tree
    // root, so existing broken installs self-correct on upgrade. Uses the same
    // $HOME source as the router's match-time guard (captured once in `Config`)
    // so heal and guard agree on its meaning.
    let healed = store
        .writer
        .heal_catch_all_repo_paths(config.home_dir.clone())
        .await?;
    if healed > 0 {
        tracing::info!(
            healed,
            "healed catch-all project repo_path rows ($HOME, filesystem root, or non-git-root path)"
        );
    }

    // Run any outstanding wiki-structure migrations before the watcher starts
    // so file moves and renames are never raced by the reconciler.
    let wiki_root = config.data_dir.join("wiki");
    run_wiki_migrations(
        &store.writer,
        &store.reader,
        &wiki_root,
        &migrations::registry(),
    )
    .await
    .with_context(|| "applying wiki-structure migrations")?;

    let ws = store
        .writer
        .get_or_create_workspace(args.workspace.clone())
        .await?;
    let proj = store
        .writer
        .get_or_create_project(ws, args.project.clone(), None)
        .await?;
    // Build the privacy strip from config. Compile errors in
    // user-supplied regex abort startup with a clear message so
    // operators discover misconfiguration immediately.
    let sanitizer = Sanitizer::new(&config.sanitize)
        .context("compiling sanitizer.extra_patterns from config")?;
    let wiki = Wiki::new(&config.data_dir, store.writer.clone())?
        .with_sanitizer(sanitizer.clone())
        // Reader attached unconditionally: admission name-resolution uses it
        // when a chain is configured, and the startup scope-manifest backfill
        // (below) always needs it to enumerate scopes.
        .with_store_reader(store.reader.clone());
    // Attach the admission webhook chain (operator-configured via
    // `[[admission_webhooks]]` in config.toml or `ENGRAM_ADMISSION_WEBHOOKS__N__*`
    // env vars). Empty config = no chain attached, zero overhead. The store
    // reader is forwarded so the chain can resolve workspace_id/project_id
    // into the human names webhooks address pages by.
    let wiki = if config.admission_webhooks.is_empty() {
        wiki
    } else {
        let chain = engram_wiki::AdmissionChain::new(config.admission_webhooks.clone())
            .context("building admission webhook chain")?;
        tracing::info!(
            count = config.admission_webhooks.len(),
            "admission webhook chain attached"
        );
        wiki.with_admission_chain(chain)
    };
    let provider_health = ProviderHealth::default();
    let (wiki, embedder) = configure_embedder(config, &store, wiki, &provider_health).await?;

    // Make the wiki tree self-describing: write each scope's `_meta.md`
    // (workspace/project name + repo_path) if missing, so the markdown alone
    // can rebuild the index via `engram reindex`. Idempotent; non-fatal.
    match wiki.backfill_scope_manifests().await {
        Ok(0) => {}
        Ok(n) => tracing::info!(count = n, "wrote _meta.md scope manifests"),
        Err(e) => tracing::warn!(error = %e, "scope-manifest backfill failed (non-fatal)"),
    }
    match wiki.ensure_upgrade_baseline_checkpoint() {
        Ok(Some(oid)) => {
            tracing::info!(checkpoint = %oid, "created wiki upgrade baseline checkpoint")
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(error = %e, "wiki upgrade baseline checkpoint failed (non-fatal)"),
    }

    // Keep the guard alive for the lifetime of `serve`.
    let _watcher = start_watcher(&args, &wiki)?;

    // Shared between the MCP server and the hook router: the hook
    // router publishes the cwd-resolved project on each event; the MCP
    // read tools read it as their default so a shared HTTP server
    // answers for the project the agent is actually in, not the static
    // `--project` (issue #2). In stdio mode no hook router is built, so
    // this stays empty and the baked-in default is used.
    // Construct ActiveProject with the configured `[auto_scope]` mode +
    // TTL/cap. `single` (default) preserves the legacy behaviour; the
    // opt-in modes (`per_session`, `per_actor`) keyed-isolate concurrent
    // sessions / operators on shared installs.
    let active_project = ActiveProject::with_config(
        config.auto_scope.mode,
        std::time::Duration::from_secs(config.auto_scope.session_ttl_secs),
        config.auto_scope.max_entries,
    );
    tracing::info!(
        mode = ?config.auto_scope.mode,
        session_ttl_secs = config.auto_scope.session_ttl_secs,
        max_entries = config.auto_scope.max_entries,
        "active-project isolation mode"
    );
    let mut server = EngramServer::new(store.reader.clone(), store.writer.clone(), ws, proj)
        .with_wiki(wiki.clone())
        .with_decay_params(config.decay)
        .with_auto_improve_require_approval(config.auto_improve.require_approval)
        .with_auto_improve_review_config(auto_improve_review_config_from_settings(
            &config.auto_improve,
        ))
        .with_active_project(active_project.clone())
        .with_sanitizer(sanitizer.clone());
    if let Some(e) = embedder.clone() {
        server = server.with_embedder(e);
    }
    let consolidator_setup =
        configure_consolidator(config, server, &store, &wiki, ws, proj, &provider_health)?;
    let server = consolidator_setup.server;
    let consolidator = consolidator_setup.consolidator;
    let admin_llm = consolidator_setup.admin_llm;
    let _maintenance_tasks = start_maintenance_scheduler(
        config.maintenance.clone(),
        config.auto_improve.clone(),
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        embedder.clone(),
        admin_llm.clone(),
        ws,
        proj,
        config.decay,
    )
    .await;

    match args.transport {
        TransportKind::Stdio => {
            info!("MCP server ready on stdio (Ctrl-C to stop)");
            let service = server.serve(stdio()).await?;
            service.waiting().await?;
        }
        TransportKind::Http => {
            let bind = args.bind.unwrap_or_else(|| config.bind.clone());
            let cancel = CancellationToken::new();
            let server_clone = server.clone();
            // `Host`-header allowlist for the HTTP DNS-rebinding guard.
            // Sourced from Config (which already handles the
            // `ENGRAM_ALLOWED_HOSTS=a,b,c` env-string vs.
            // config.toml sequence forms via the string-or-vec
            // deserializer). Logged so operators can verify the
            // effective list against what they intended.
            info!(
                allowed_hosts = ?config.allowed_hosts,
                "HTTP Host-header allowlist"
            );
            // Default to stateless Streamable HTTP: each POST is serviced
            // independently and answered as plain `application/json`, so
            // stateless clients (OpenCode `type: "remote"`, curl) work
            // without an `mcp-remote` shim (issue #3). engram's tools
            // are pure request-response and project resolution rides the
            // in-process `ActiveProject` pointer, not the transport
            // session — so session mode buys us nothing. `--http-stateful`
            // restores rmcp's session+SSE behaviour for clients that want
            // it.
            info!(
                stateful = args.http_stateful,
                "MCP Streamable HTTP transport mode"
            );
            let mcp_service = StreamableHttpService::new(
                move || Ok(server_clone.clone()),
                LocalSessionManager::default().into(),
                StreamableHttpServerConfig::default()
                    .with_cancellation_token(cancel.child_token())
                    .with_allowed_hosts(config.allowed_hosts.clone())
                    .with_stateful_mode(args.http_stateful)
                    .with_json_response(!args.http_stateful),
            );
            // Shared per-cwd project cache: the hook router owns it; the admin
            // router gets a fire-and-forget eviction hook so a `move-project`
            // can proactively drop the moved project's stale entries.
            let project_cache: engram_hooks::ProjectCache =
                std::sync::Arc::new(tokio::sync::Mutex::new(ProjectCacheStore::default()));
            let on_project_moved: std::sync::Arc<dyn Fn(ProjectId) + Send + Sync> = {
                let cache = project_cache.clone();
                std::sync::Arc::new(move |proj: ProjectId| {
                    let cache = cache.clone();
                    tokio::spawn(async move {
                        cache.lock().await.retain(|_, v| v.1 != proj);
                    });
                })
            };
            let hooks = hook_router(HookState {
                workspace_id: ws,
                project_id: proj,
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                wiki: wiki.clone(),
                consolidator: consolidator.clone(),
                sanitizer: sanitizer.clone(),
                project_cache: project_cache.clone(),
                active_project: active_project.clone(),
                ingest_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(
                    DEFAULT_HOOK_INGEST_MAX_IN_FLIGHT,
                )),
                consolidate_on_session_end: config.consolidate_on_session_end,
                subagent_sessions: std::sync::Arc::new(tokio::sync::Mutex::new(
                    engram_hooks::SubagentSessionSet::default(),
                )),
                home_dir: config.home_dir.clone(),
            });
            let admin = admin_router(AdminState {
                writer: store.writer.clone(),
                reader: store.reader.clone(),
                wiki: wiki.clone(),
                llm: admin_llm,
                auto_improve_require_approval: config.auto_improve.require_approval,
                auto_improve_review_config: auto_improve_review_config_from_settings(
                    &config.auto_improve,
                ),
                embedder: embedder.clone(),
                provider_health: provider_health.clone(),
                decay_params: config.decay,
                data_dir: config.data_dir.clone(),
                db_path: store.db_path().to_path_buf(),
                bind: bind.clone(),
                home_dir: config.home_dir.clone(),
                bootstrap_lock: std::sync::Arc::new(tokio::sync::Mutex::new(())),
                token_pepper: config
                    .auth
                    .token_pepper
                    .as_ref()
                    .filter(|p| !p.trim().is_empty())
                    .map(|p| engram_store::TokenPepper::new(p.clone())),
                active_project: active_project.clone(),
                on_project_moved: Some(on_project_moved),
            });
            // Multi-rung auth assembly:
            //   - rung 0 (no bearer_token configured) → AuthState::new
            //     stays as-is, middleware injects anonymous actor.
            //   - rung 1 (bearer_token set, no token_pepper) → root_actor
            //     stamps writes with [auth].root_* identity.
            //   - rung 2 (bearer_token + token_pepper + users in DB) →
            //     unknown bearer routes through users-table lookup.
            // The pepper is auto-generated by `engram init`, so almost
            // every operator-installed server reaches rung 2; the only
            // rung-1-only setups are those whose config predates v0.8.
            let mut auth_state = AuthState::new(config.auth.bearer_token.clone());
            let root_user = config.auth.root_username.clone();
            if root_user.as_deref().is_some_and(|s| !s.trim().is_empty()) {
                auth_state = auth_state.with_root_actor(engram_core::ActorContext {
                    user: root_user,
                    name: config.auth.root_name.clone(),
                    email: config.auth.root_email.clone(),
                    ..engram_core::ActorContext::default()
                });
            }
            if let Some(pepper) = config
                .auth
                .token_pepper
                .as_ref()
                .filter(|p| !p.trim().is_empty())
            {
                auth_state = auth_state.with_multiuser(
                    engram_store::TokenPepper::new(pepper.clone()),
                    store.reader.clone(),
                    store.writer.clone(),
                );
            }
            let auth_state = Arc::new(auth_state);
            let auth_enabled = auth_state.enabled();
            let router = axum::Router::new()
                .nest_service("/mcp", mcp_service)
                .merge(hooks)
                .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
                .merge(admin.layer(DefaultBodyLimit::max(BOOTSTRAP_MAX_BODY_BYTES)));
            let base_path = normalize_prefix(&args.base_path);
            if base_path.is_empty() && !args.base_path.trim_matches('/').trim().is_empty() {
                tracing::warn!(
                    raw = %args.base_path,
                    "ENGRAM_BASE_PATH is not a safe path prefix; serving at root instead",
                );
            }
            // Symmetric warning for `--web-slug` / `ENGRAM_WEB_SLUG`. The
            // original commit only warned on `base_path` downgrade, so an
            // operator who set `ENGRAM_WEB_SLUG=/web space` would have
            // their slug silently collapsed to the empty mount with no
            // signal — same hazard as base-path.
            let web_slug_normal = normalize_prefix(&args.web_slug);
            if web_slug_normal.is_empty() && !args.web_slug.trim_matches('/').trim().is_empty() {
                tracing::warn!(
                    raw = %args.web_slug,
                    "ENGRAM_WEB_SLUG is not a safe path prefix; serving the web UI at the base-path root instead",
                );
            }
            let base_href = web_base_href(&args.base_path, &args.web_slug);
            let router = mount_web_router(
                router,
                args.enable_web,
                store.reader.clone(),
                wiki.clone(),
                WebMountSpec {
                    web_ui_dir: args.web_ui_dir.as_deref(),
                    cors_origins: &cors_origins,
                    web_slug: &args.web_slug,
                    base_href: &base_href,
                    base_path: &base_path,
                },
            )?;
            let router = apply_http_layers(router, auth_state, config.allowed_hosts.clone());
            // Host the entire surface under the configured base path. Empty
            // base = root (unchanged). The auth/host layers are already
            // attached to `router`, so they run for every nested route.
            let router = if base_path.is_empty() {
                router
            } else {
                axum::Router::new().nest(&base_path, router)
            };
            // Mount `/favicon.ico` at the absolute HOST root — outside the
            // auth gate, outside `--base-path`, outside the `/web` nest.
            // Browsers auto-fetch `<host>/favicon.ico` without auth headers
            // regardless of where the app is mounted; routing it under
            // `/web` (as PR #79 originally did) made it unreachable to the
            // browser's automatic fetch. Negligible info leak — the icon
            // is the same embedded PNG anyone hitting `/web` already sees.
            let router = if args.enable_web {
                router.merge(engram_web::favicon_router())
            } else {
                router
            };
            let listener = tokio::net::TcpListener::bind(&bind)
                .await
                .with_context(|| format!("binding {bind}"))?;
            info!(
                %bind,
                auth = auth_enabled,
                body_limit_mb = MAX_BODY_BYTES / 1024 / 1024,
                "MCP HTTP server ready (POST /mcp, POST /hook, Ctrl-C to stop)",
            );
            if !auth_enabled && !bind.starts_with("127.") {
                // Loud warning: a non-loopback bind with no auth is
                // the audit's critical-#1 scenario. The operator gets
                // a one-line "you sure?" instead of silent exposure.
                tracing::warn!(
                    %bind,
                    "no ENGRAM_AUTH_TOKEN configured AND binding to a non-loopback \
                     address — anyone on the network can call destructive MCP tools. \
                     Generate a token with `engram generate-auth-token` and set \
                     ENGRAM_AUTH_TOKEN in the server's environment."
                );
            } else if auth_enabled && !bind.starts_with("127.") {
                // Auth IS configured but the server is reachable from
                // the network on plain HTTP. The bearer token (and
                // multi-user per-user tokens from `engram user
                // add`) ride cleartext — sniffable on the LAN. Advise
                // the operator to front with a TLS proxy. One-shot
                // log at startup, not refusal to serve (operators may
                // be testing, behind their own proxy already, etc.).
                tracing::warn!(
                    %bind,
                    "ENGRAM_AUTH_TOKEN is set but the server is bound to a \
                     non-loopback address on plain HTTP — bearer tokens travel \
                     cleartext on the network. Front engram with a TLS-terminating \
                     reverse proxy (Caddy, Cloudflare Tunnel, nginx). See \
                     docs/https-via-proxy.md for copy-paste templates."
                );
            }
            axum::serve(listener, router)
                .with_graceful_shutdown(async move {
                    let _ = tokio::signal::ctrl_c().await;
                    info!("ctrl-c received; shutting down");
                    cancel.cancel();
                })
                .await?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn start_maintenance_scheduler(
    settings: MaintenanceSettings,
    auto_improve: AutoImproveSettings,
    reader: ReaderPool,
    writer: WriterHandle,
    wiki: Wiki,
    embedder: Option<Arc<dyn Embedder>>,
    llm: Option<Arc<dyn LlmProvider>>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    decay: engram_store::DecayParams,
) -> Vec<tokio::task::JoinHandle<()>> {
    let maintenance_enabled = settings.enabled;
    if !maintenance_enabled {
        info!("scheduled retention/lint/embed maintenance disabled");
    }

    let forget_sweep_interval_secs = settings.forget_sweep_interval_secs;
    let lint_interval_secs = settings.lint_interval_secs;
    let embedding_backfill_interval_secs = settings.embedding_backfill_interval_secs;

    let mut tasks = Vec::new();
    if maintenance_enabled && forget_sweep_interval_secs > 0 {
        let reader = reader.clone();
        let writer = writer.clone();
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(forget_sweep_interval_secs);
            loop {
                tokio::time::sleep(interval).await;
                match run_sweep(&reader, &writer, workspace_id, project_id, &decay, false).await {
                    Ok(report) => info!(
                        evicted = report.evicted.len(),
                        hard_deleted = report.hard_deleted,
                        "scheduled forget sweep completed"
                    ),
                    Err(e) => tracing::warn!(error = %e, "scheduled forget sweep failed"),
                }
            }
        }));
    }

    // Hollow-project sweep: deletes project rows with zero data of any
    // kind (pages, sessions, observations, handoffs) once they are older
    // than HOLLOW_PROJECT_MIN_AGE_DAYS. Safe by construction — nothing
    // exists to lose — which is why it runs unconditionally under the
    // maintenance flag with no extra config. Runs once shortly after
    // startup (so upgrades clean up immediately) and then daily.
    if maintenance_enabled {
        /// A week of grace before a hollow row is considered noise, so a
        /// project created moments before its first real event is never
        /// racing the sweep.
        const HOLLOW_PROJECT_MIN_AGE_DAYS: u32 = 7;
        const HOLLOW_SWEEP_INTERVAL: std::time::Duration =
            std::time::Duration::from_secs(24 * 60 * 60);
        /// Short startup delay so the sweep never competes with migration
        /// and first-request work on boot.
        const HOLLOW_SWEEP_STARTUP_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
        let writer = writer.clone();
        tasks.push(tokio::spawn(async move {
            tokio::time::sleep(HOLLOW_SWEEP_STARTUP_DELAY).await;
            loop {
                match writer
                    .sweep_hollow_projects(HOLLOW_PROJECT_MIN_AGE_DAYS)
                    .await
                {
                    Ok(deleted) if deleted.is_empty() => {}
                    Ok(deleted) => info!(
                        count = deleted.len(),
                        projects = deleted.join(", "),
                        "hollow-project sweep deleted empty project rows"
                    ),
                    Err(e) => tracing::warn!(error = %e, "hollow-project sweep failed"),
                }
                tokio::time::sleep(HOLLOW_SWEEP_INTERVAL).await;
            }
        }));
    }

    if maintenance_enabled && lint_interval_secs > 0 {
        let reader = reader.clone();
        let wiki = wiki.clone();
        let llm = llm.clone();
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(lint_interval_secs);
            loop {
                tokio::time::sleep(interval).await;
                match run_lint(
                    &reader,
                    &wiki,
                    llm.as_ref(),
                    workspace_id,
                    project_id,
                    false,
                    false,
                )
                .await
                {
                    Ok(report) => info!(
                        findings = report.findings.len(),
                        "scheduled rule-based lint completed"
                    ),
                    Err(e) => tracing::warn!(error = %e, "scheduled lint failed"),
                }
            }
        }));
    }

    if maintenance_enabled && embedding_backfill_interval_secs > 0 {
        if let Some(embedder) = embedder {
            let reader = reader.clone();
            let writer = writer.clone();
            let wiki = wiki.clone();
            tasks.push(tokio::spawn(async move {
                let interval = std::time::Duration::from_secs(embedding_backfill_interval_secs);
                loop {
                    tokio::time::sleep(interval).await;
                    match run_embedding_backfill(
                        &reader,
                        &writer,
                        &wiki,
                        &embedder,
                        workspace_id,
                        project_id,
                    )
                    .await
                    {
                        Ok((embedded, failed)) => {
                            info!(embedded, failed, "scheduled embedding backfill completed")
                        }
                        Err(e) => tracing::warn!(error = %e, "scheduled embedding backfill failed"),
                    }
                }
            }));
        } else {
            tracing::warn!(
                "maintenance.embedding_backfill_interval_secs is set but no embedder is configured"
            );
        }
    }

    let scheduler = auto_improve.scheduler.clone();
    if !scheduler.enabled || scheduler.interval_secs == 0 || scheduler.max_sessions_per_tick == 0 {
        info!("auto-improve scheduler disabled; manual auto-improve remains available");
    } else if let Some(llm) = llm.clone() {
        let reader = reader.clone();
        let writer = writer.clone();
        let wiki = wiki.clone();
        match initialize_auto_improve_scheduler_scopes(&reader, &writer).await {
            Ok((scopes, errors)) => info!(
                scopes,
                errors, "auto-improve scheduler startup scope initialization completed"
            ),
            Err(e) => tracing::warn!(
                error = %e,
                "auto-improve scheduler startup scope initialization failed"
            ),
        }
        tasks.push(tokio::spawn(async move {
            let interval = std::time::Duration::from_secs(scheduler.interval_secs);
            // Sleep after each complete tick instead of driving work from a
            // fixed-rate interval. If reviewing all projects takes longer than
            // `interval`, the next tick is delayed rather than overlapping the
            // still-running one.
            loop {
                tokio::time::sleep(interval).await;
                let started = std::time::Instant::now();
                match run_auto_improve_scheduler_tick(&reader, &writer, &wiki, &llm, &auto_improve)
                    .await
                {
                    Ok(outcome) => info!(
                        scopes = outcome.scopes,
                        scopes_with_candidates = outcome.scopes_with_candidates,
                        reviewed = outcome.reviewed,
                        errors = outcome.errors,
                        elapsed_ms = started.elapsed().as_millis(),
                        "scheduled auto-improve tick completed"
                    ),
                    Err(e) => {
                        tracing::warn!(error = %e, "scheduled auto-improve tick failed")
                    }
                };
            }
        }));
    } else {
        info!("auto-improve scheduler enabled but no LLM provider is configured; job not started");
    }

    if tasks.is_empty() {
        info!("scheduled maintenance enabled but all intervals are disabled");
    } else {
        info!(jobs = tasks.len(), "scheduled maintenance started");
    }
    tasks
}

async fn initialize_auto_improve_scheduler_scopes(
    reader: &ReaderPool,
    writer: &WriterHandle,
) -> Result<(usize, usize)> {
    let scopes = reader.list_all_scopes().await?;
    let total = scopes.len();
    let mut errors = 0usize;
    for scope in scopes {
        if let Err(e) = writer
            .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
            .await
        {
            errors += 1;
            tracing::warn!(
                workspace = %scope.workspace_name,
                project = %scope.project_name,
                error = %e,
                "auto-improve scheduler startup state init failed"
            );
        }
    }
    Ok((total, errors))
}

async fn run_embedding_backfill(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    embedder: &Arc<dyn Embedder>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
) -> Result<(usize, usize)> {
    let provider = embedder.provider().to_string();
    let model = embedder.model().to_string();
    let dim = embedder.dim();
    let candidates = reader.decay_candidates(workspace_id, project_id).await?;
    let already: std::collections::HashSet<_> = reader
        .fully_embedded_page_ids(
            workspace_id,
            project_id,
            provider.clone(),
            model.clone(),
            dim,
            engram_llm::DOC_CHUNK_MAX_BYTES as u64,
        )
        .await?
        .into_iter()
        .collect();

    let mut embedded = 0usize;
    let mut failed = 0usize;
    let mut pending = Vec::with_capacity(EMBEDDING_WRITE_BATCH);
    for cand in candidates {
        if already.contains(&cand.id) {
            continue;
        }
        let md = match wiki.read_page(workspace_id, project_id, &cand.path) {
            Ok(md) => md,
            Err(e) => {
                failed += 1;
                tracing::warn!(path = %cand.path, error = %e, "scheduled embed: unreadable page");
                continue;
            }
        };
        let vecs = match embedder.embed_document_chunked(&md.body).await {
            Ok(vecs) => vecs,
            Err(e) => {
                failed += 1;
                tracing::warn!(path = %cand.path, error = %e, "scheduled embed: provider failed");
                continue;
            }
        };
        pending.push(EmbeddingWrite {
            page_id: cand.id,
            vectors: vecs.iter().map(|v| f32_vec_to_bytes(v)).collect(),
            provider: provider.clone(),
            model: model.clone(),
            dim,
        });
        if pending.len() >= EMBEDDING_WRITE_BATCH {
            flush_embedding_batch(writer, &mut pending, &mut embedded, &mut failed).await;
        }
    }
    flush_embedding_batch(writer, &mut pending, &mut embedded, &mut failed).await;
    Ok((embedded, failed))
}

async fn flush_embedding_batch(
    writer: &WriterHandle,
    pending: &mut Vec<EmbeddingWrite>,
    embedded: &mut usize,
    failed: &mut usize,
) {
    if pending.is_empty() {
        return;
    }
    let batch = std::mem::replace(pending, Vec::with_capacity(EMBEDDING_WRITE_BATCH));
    let count = batch.len();
    if let Err(e) = writer.store_embeddings(batch).await {
        *failed += count;
        tracing::warn!(count, error = %e, "scheduled embed: batch store failed");
    } else {
        *embedded += count;
    }
}

struct ScheduledAutoImproveOutcome {
    run_id: engram_core::AutoImproveRunId,
    proposals: usize,
    approved: usize,
    pending: usize,
    conflicts: usize,
    skipped: usize,
}

#[derive(Debug, Default)]
struct ScheduledAutoImproveTickOutcome {
    scopes: usize,
    scopes_with_candidates: usize,
    reviewed: usize,
    errors: usize,
}

struct ScheduledAutoImproveContext<'a> {
    reader: &'a ReaderPool,
    writer: &'a WriterHandle,
    wiki: &'a Wiki,
    llm: &'a Arc<dyn LlmProvider>,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    settings: &'a AutoImproveSettings,
}

async fn run_auto_improve_scheduler_tick(
    reader: &ReaderPool,
    writer: &WriterHandle,
    wiki: &Wiki,
    llm: &Arc<dyn LlmProvider>,
    settings: &AutoImproveSettings,
) -> Result<ScheduledAutoImproveTickOutcome> {
    let scopes = reader.list_all_scopes().await?;
    let mut outcome = ScheduledAutoImproveTickOutcome {
        scopes: scopes.len(),
        ..ScheduledAutoImproveTickOutcome::default()
    };

    for scope in scopes {
        if let Err(e) = writer
            .ensure_auto_improve_scheduler_state(scope.workspace_id, scope.project_id)
            .await
        {
            outcome.errors += 1;
            tracing::warn!(
                workspace = %scope.workspace_name,
                project = %scope.project_name,
                error = %e,
                "scheduled auto-improve state init failed"
            );
            continue;
        }

        let candidates = match reader
            .auto_improve_candidate_sessions(
                scope.workspace_id,
                scope.project_id,
                settings.scheduler.min_session_age_secs,
                settings.scheduler.max_sessions_per_tick,
            )
            .await
        {
            Ok(candidates) => candidates,
            Err(e) => {
                outcome.errors += 1;
                tracing::warn!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    error = %e,
                    "scheduled auto-improve candidate query failed"
                );
                continue;
            }
        };
        if candidates.is_empty() {
            continue;
        }

        outcome.scopes_with_candidates += 1;
        let ctx = ScheduledAutoImproveContext {
            reader,
            writer,
            wiki,
            llm,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            settings,
        };
        for candidate in candidates {
            let claimed = match ctx
                .writer
                .claim_auto_improve_scheduler_session(
                    ctx.workspace_id,
                    ctx.project_id,
                    candidate.session_id,
                    candidate.ended_at,
                )
                .await
            {
                Ok(claimed) => claimed,
                Err(e) => {
                    outcome.errors += 1;
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        error = %e,
                        "scheduled auto-improve claim failed"
                    );
                    continue;
                }
            };
            if !claimed {
                tracing::debug!(
                    workspace = %scope.workspace_name,
                    project = %scope.project_name,
                    session_id = %candidate.session_id,
                    "scheduled auto-improve candidate already claimed or reviewed"
                );
                continue;
            }
            match run_scheduled_auto_improve(&ctx, candidate.session_id).await {
                Ok(run) => {
                    outcome.reviewed += 1;
                    info!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        run_id = %run.run_id,
                        proposals = run.proposals,
                        approved = run.approved,
                        pending = run.pending,
                        conflicts = run.conflicts,
                        skipped = run.skipped,
                        "scheduled auto-improve completed"
                    );
                }
                Err(e) => {
                    outcome.errors += 1;
                    tracing::warn!(
                        workspace = %scope.workspace_name,
                        project = %scope.project_name,
                        session_id = %candidate.session_id,
                        error = %e,
                        "scheduled auto-improve failed"
                    );
                }
            }
        }
    }

    Ok(outcome)
}

async fn run_scheduled_auto_improve(
    ctx: &ScheduledAutoImproveContext<'_>,
    session_id: SessionId,
) -> Result<ScheduledAutoImproveOutcome> {
    let cfg = auto_improve_review_config_from_settings(ctx.settings);
    let report = run_auto_improve_review(
        ctx.reader,
        &**ctx.llm,
        ctx.workspace_id,
        ctx.project_id,
        session_id,
        cfg.clone(),
    )
    .await?;
    let proposals =
        scheduled_auto_improve_new_proposals(ctx.reader, ctx.workspace_id, ctx.project_id, &report)
            .await?;
    let staged = ctx
        .writer
        .stage_auto_improve_run(StageAutoImproveRun {
            workspace_id: ctx.workspace_id,
            project_id: ctx.project_id,
            session_id: Some(session_id),
            provider: Some(report.provider.clone()),
            model: Some(report.model.clone()),
            summary: Some(report.summary.clone()),
            warnings_json: serde_json::to_value(&report.warnings)
                .unwrap_or_else(|_| serde_json::json!([])),
            rejected_candidates_json: serde_json::to_value(&report.rejected_candidates)
                .unwrap_or_else(|_| serde_json::json!([])),
            config_json: serde_json::json!({
                "trigger": "scheduler",
                "min_observations": cfg.min_observations,
                "min_session_duration_secs": cfg.min_session_duration_secs,
                "min_confidence": cfg.min_confidence,
                "max_input_tokens": cfg.max_input_tokens,
                "max_proposals_per_run": cfg.max_proposals_per_run,
                "include_raw_fallback": cfg.include_raw_fallback,
                "max_patchable_pages": cfg.max_patchable_pages,
                "max_patchable_body_chars": cfg.max_patchable_body_chars,
                "max_edits_per_proposal": cfg.max_edits_per_proposal,
                "max_edit_content_chars": cfg.max_edit_content_chars,
                "max_changed_chars_per_proposal": cfg.max_changed_chars_per_proposal,
                "max_patch_edits_per_run": cfg.max_patch_edits_per_run,
                "max_rejection_context": cfg.max_rejection_context,
                "rejection_context_days": cfg.rejection_context_days,
                "max_final_body_chars": cfg.max_final_body_chars,
                "max_rule_page_tokens": cfg.max_rule_page_tokens,
                "max_procedure_page_tokens": cfg.max_procedure_page_tokens,
                "eval": cfg.eval,
                "require_approval": ctx.settings.require_approval,
            }),
            proposal_actor: ActorContext {
                agent: Some(cfg.proposal_actor.clone()),
                ..ActorContext::default()
            },
            proposals,
        })
        .await?;

    for id in &staged.proposal_ids {
        ctx.wiki
            .write_auto_improve_sidecar(ctx.workspace_id, ctx.project_id, *id)
            .await?;
    }

    let mut approved = 0usize;
    let mut pending = 0usize;
    let mut conflicts = 0usize;
    for proposal_id in &staged.proposal_ids {
        if ctx.settings.require_approval {
            pending += 1;
            continue;
        }
        match ctx
            .wiki
            .approve_auto_improve_proposal(
                ctx.workspace_id,
                ctx.project_id,
                *proposal_id,
                ActorContext {
                    agent: Some("auto_improve_scheduler_auto_approve".into()),
                    ..ActorContext::default()
                },
                None,
                Some(engram_wiki::AdmissionContext {
                    op: engram_wiki::AdmissionOp::WritePage,
                    ..engram_wiki::AdmissionContext::default()
                }),
            )
            .await?
        {
            ApproveAutoImproveProposalResult::Approved { .. } => approved += 1,
            ApproveAutoImproveProposalResult::Conflict => conflicts += 1,
        }
    }

    for skip in &staged.skipped {
        tracing::info!(
            run_id = %staged.run_id,
            target = %skip.target_path,
            reason = %skip.reason,
            "scheduled auto-improve proposal skipped"
        );
    }

    Ok(ScheduledAutoImproveOutcome {
        run_id: staged.run_id,
        proposals: staged.proposal_ids.len(),
        approved,
        pending,
        conflicts,
        skipped: staged.skipped.len(),
    })
}

fn auto_improve_review_config_from_settings(
    settings: &AutoImproveSettings,
) -> AutoImproveReviewConfig {
    AutoImproveReviewConfig {
        min_observations: settings.min_observations,
        min_session_duration_secs: settings.min_session_duration_secs,
        min_confidence: settings.min_confidence,
        max_input_tokens: settings.max_input_tokens,
        max_proposals_per_run: settings.max_proposals_per_run,
        include_raw_fallback: settings.include_raw_fallback,
        proposal_actor: settings.proposal_actor.clone(),
        pending_path: settings.pending_path.clone(),
        max_patchable_pages: settings.max_patchable_pages,
        max_patchable_body_chars: settings.max_patchable_body_chars,
        max_edits_per_proposal: settings.max_edits_per_proposal,
        max_edit_content_chars: settings.max_edit_content_chars,
        max_changed_chars_per_proposal: settings.max_changed_chars_per_proposal,
        max_patch_edits_per_run: settings.max_patch_edits_per_run,
        max_rejection_context: settings.max_rejection_context,
        rejection_context_days: settings.rejection_context_days,
        max_final_body_chars: settings.max_final_body_chars,
        max_rule_page_tokens: settings.max_rule_page_tokens,
        max_procedure_page_tokens: settings.max_procedure_page_tokens,
        eval: engram_consolidate::AutoImproveEvalConfig {
            enabled: settings.eval.enabled,
            command: settings.eval.command.clone(),
            timeout_secs: settings.eval.timeout_secs,
            targets: settings.eval.targets.clone(),
            min_delta: settings.eval.min_delta,
        },
    }
}

async fn scheduled_auto_improve_new_proposals(
    reader: &ReaderPool,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    report: &engram_consolidate::AutoImproveReport,
) -> Result<Vec<NewAutoImproveProposal>> {
    let mut proposals = Vec::with_capacity(report.proposals.len());
    for p in &report.proposals {
        let path = PagePath::new(p.path.clone())?;
        let target_exists = reader
            .page_body_by_ids(workspace_id, project_id, path.as_str())
            .await?
            .is_some();
        let operation = if p.edit_mode == "patch"
            || (target_exists && path.as_str() == "_slots/current-focus.md")
        {
            AutoImproveProposalOperation::Update
        } else {
            AutoImproveProposalOperation::Create
        };
        let expected_base_body_sha256 = p
            .expected_base_body_sha256
            .as_deref()
            .map(hex_to_sha256)
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid expected_base_body_sha256: {e}"))?;
        proposals.push(NewAutoImproveProposal {
            operation,
            target_path: path,
            kind: p.kind.clone(),
            title: p.title.clone(),
            confidence: f64::from(p.confidence),
            rationale: p.rationale.clone(),
            evidence_json: serde_json::to_value(&p.evidence)
                .unwrap_or_else(|_| serde_json::json!([])),
            body_markdown: p.body_markdown.clone(),
            artifact_sha256: None,
            edit_mode: Some(p.edit_mode.clone()),
            patch_json: serde_json::to_value(&p.edits).ok(),
            expected_base_body_sha256,
        });
    }
    Ok(proposals)
}

fn hex_to_sha256(hex: &str) -> Result<[u8; 32], String> {
    if hex.len() != 64 {
        return Err("expected 64 hex chars".into());
    }
    let mut out = [0_u8; 32];
    for (idx, chunk) in hex.as_bytes().chunks_exact(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|e| e.to_string())?;
        out[idx] = u8::from_str_radix(s, 16).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

async fn configure_embedder(
    config: &Config,
    store: &Store,
    wiki: Wiki,
    provider_health: &ProviderHealth,
) -> Result<(Wiki, Option<Arc<dyn Embedder>>)> {
    // M9 — pluggable embedder. Stored rows carry provider/model/dim so
    // query paths can ignore stale vectors after an embedding config change.
    let Some(cfg) = config.embedder_config()? else {
        info!("ENGRAM_EMBEDDING_PROVIDER unset; hybrid search disabled (FTS5-only)");
        return Ok((wiki, None));
    };
    let provider_name = cfg.provider.name().to_string();
    let model = cfg.model.clone();
    let dim = cfg.dim;
    let embedder = build_embedder(cfg).context("building embedder from config")?;
    let mismatch = store
        .reader
        .embedding_meta_for_mismatch(
            embedder.provider().into(),
            embedder.model().into(),
            embedder.dim(),
        )
        .await?;
    if !mismatch.is_empty() {
        // Mismatch handling applies to hybrid search (queries only load
        // rows matching the configured triple), not to process liveness.
        // Blocking startup made `embed --force` impossible because the
        // CLI is an HTTP client to this server.
        tracing::warn!(
            stored = ?mismatch,
            configured_provider = embedder.provider(),
            configured_model = embedder.model(),
            configured_dim = embedder.dim(),
            "stored embeddings use a different (provider, model, dim) than configured; \
             hybrid search ignores stale rows until pages are re-embedded — \
             run `engram embed --force` (or wait for scheduled backfill)"
        );
    }
    info!(
        provider = embedder.provider(),
        model = embedder.model(),
        dim = embedder.dim(),
        "embedder enabled"
    );
    let embedder = provider_health.wrap_embedder(embedder, provider_name, model, dim);
    Ok((wiki.with_embedder(embedder.clone()), Some(embedder)))
}

fn start_watcher(args: &ServeArgs, wiki: &Wiki) -> Result<Option<WatcherHandle>> {
    if args.no_watcher {
        info!("watcher disabled by --no-watcher");
        return Ok(None);
    }
    info!(
        root = %wiki.root().display(),
        workspace = %args.workspace,
        project = %args.project,
        "starting wiki watcher",
    );
    Ok(Some(WatcherHandle::start(wiki.clone())?))
}

fn configure_consolidator(
    config: &Config,
    mut server: EngramServer,
    store: &Store,
    wiki: &Wiki,
    workspace_id: WorkspaceId,
    project_id: ProjectId,
    provider_health: &ProviderHealth,
) -> Result<ConsolidatorSetup> {
    // Build the consolidator (if LLM configured) once, then share the
    // Arc between the MCP server (for `memory_consolidate` + lint),
    // the hook router (for PreCompact checkpointing), and the admin
    // router (for `POST /admin/bootstrap`).
    let Some(cfg) = config.llm_provider_config()? else {
        info!(
            "ENGRAM_LLM_PROVIDER unset; memory_consolidate disabled, PreCompact \
             falls back to rule-based checkpoint, lint runs rule-based only"
        );
        return Ok(ConsolidatorSetup {
            server,
            consolidator: None,
            admin_llm: None,
        });
    };
    let provider_name = cfg.provider.name().to_string();
    let model = cfg.model.clone();
    let retry_hint = llm_retry_hint(&provider_name, &model, cfg.base_url.as_deref());
    let llm = build_provider(cfg).context("building LLM provider from config")?;
    let llm = provider_health.wrap_llm_provider(llm, provider_name, model, Some(retry_hint));
    info!(
        provider = llm.name(),
        model = llm.model(),
        "memory_consolidate + PreCompact LLM checkpointing enabled",
    );
    let consolidator = Arc::new(Consolidator::new(
        store.reader.clone(),
        store.writer.clone(),
        wiki.clone(),
        llm.clone(),
        workspace_id,
        project_id,
    ));
    server = server.with_consolidator_arc(wiki.clone(), llm.clone(), consolidator.clone());
    Ok(ConsolidatorSetup {
        server,
        consolidator: Some(consolidator),
        admin_llm: Some(llm),
    })
}

/// Validate a list of CORS origins before the server binds.
///
/// Rules match the spec: wildcard + credentials is forbidden, each entry
/// must carry a scheme, and trailing slashes are rejected (they do not
/// match browser origins which never carry a trailing slash).
pub fn validate_cors_origins(origins: &[String]) -> Result<()> {
    for origin in origins {
        if origin == "*" {
            anyhow::bail!(
                "CORS origin `*` is not allowed: the CORS spec forbids credentials \
                 with a wildcard origin. Use explicit origins such as \
                 https://app.example.com"
            );
        }
        if !origin.starts_with("http://") && !origin.starts_with("https://") {
            anyhow::bail!(
                "CORS origin `{origin}` is missing a scheme. Each entry must start \
                 with http:// or https://"
            );
        }
        if origin.ends_with('/') {
            anyhow::bail!(
                "CORS origin `{origin}` has a trailing slash. Browser origins \
                 never carry a trailing slash — use `{}` instead",
                origin.trim_end_matches('/')
            );
        }
    }
    Ok(())
}

/// Merge config-file origins with CLI flag origins, preserving order and
/// deduplicating (config entries appear first).
fn merge_cors_origins(from_config: &[String], from_cli: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut merged = Vec::new();
    for origin in from_config.iter().chain(from_cli.iter()) {
        if seen.insert(origin.clone()) {
            merged.push(origin.clone());
        }
    }
    merged
}

fn validate_web_ui_args(enable_web: bool, web_ui_dir: Option<&Path>) -> Result<()> {
    if web_ui_dir.is_some() && !enable_web {
        anyhow::bail!("--web-ui-dir requires --enable-web");
    }

    if let Some(dir) = web_ui_dir {
        if !dir.is_dir() {
            anyhow::bail!("--web-ui-dir is not a directory: {}", dir.display());
        }
        if !dir.join("index.html").is_file() {
            anyhow::bail!("--web-ui-dir is missing index.html: {}", dir.display());
        }
    }

    Ok(())
}

fn llm_retry_hint(provider: &str, model: &str, base_url: Option<&str>) -> String {
    let mut command = format!("engram llm-test --provider {provider} --model {model}");
    if let Some(base_url) = base_url {
        command.push_str(&format!(" --base-url {base_url}"));
    }
    command.push_str(" --prompt ping");
    command
}

/// Normalise an operator-supplied path prefix into either `""` (root) or
/// `/<core>` — exactly one leading slash, no trailing slash, internal
/// empty/`//` segments collapsed.
///
/// Each segment must be a non-trivial member of the RFC 3986 *unreserved*
/// set (`ALPHA / DIGIT / "-" / "." / "_" / "~"`). Dot-segments `.` and
/// `..` are rejected outright even though their characters are unreserved
/// — at the segment level they re-encode "current directory" and "parent
/// directory" and would let a malformed env var hand the operator a
/// traversal vector through every URL the server emits.
///
/// Anything that falls outside the rule collapses to `""` (root) so a
/// bad env var can never inject markup or a protocol-relative `//` into
/// served HTML.
pub(crate) fn normalize_prefix(raw: &str) -> String {
    let segs: Vec<&str> = raw.trim().split('/').filter(|s| !s.is_empty()).collect();
    if segs.is_empty() {
        return String::new();
    }
    let safe = segs.iter().all(|s| {
        // Reject dot-segments (`.` / `..`) — they pass the per-char
        // unreserved test but mean "current" / "parent" at the segment
        // boundary and turn `/<base>` into `/<base>/..` traversal.
        *s != "." && *s != ".." && s.chars().all(is_unreserved_url_char)
    });
    if !safe {
        return String::new();
    }
    format!("/{}", segs.join("/"))
}

/// RFC 3986 `unreserved = ALPHA / DIGIT / "-" / "." / "_" / "~"`. Kept
/// here as a single source of truth for both the path-prefix charset
/// check and any future per-segment validation.
fn is_unreserved_url_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~')
}

/// Build the `<base href>` value (always trailing-slash-terminated, never
/// the protocol-relative `//`) for the web UI mounted at `base_path` +
/// `web_slug`.
pub(crate) fn web_base_href(base_path: &str, web_slug: &str) -> String {
    let combined = format!(
        "{}{}",
        normalize_prefix(base_path),
        normalize_prefix(web_slug)
    );
    if combined.is_empty() {
        "/".to_string()
    } else {
        format!("{combined}/")
    }
}

/// Escape a string for safe inclusion inside a double-quoted HTML attribute.
fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Insert `snippet` immediately after the first `<head…>` tag (or prepend
/// it when there is no head).
///
/// Matches outside HTML comments only: `<!-- … <head … --> <head>` skips
/// the comment-internal occurrence and injects after the real `<head>`.
/// Anything inside `<textarea>`, `<script>`, or other raw-text elements
/// is NOT specially handled — built-in askama templates never put
/// `<head` in those, and a custom `--web-ui-dir` SPA that does is a
/// misconfiguration the operator can fix at the source. Avoiding a
/// full HTML parser here keeps injection a single pass + alloc.
fn inject_into_head(html: &str, snippet: &str) -> String {
    let bytes = html.as_bytes();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        // Skip past any HTML comment opening here so a `<head` literal
        // sitting inside it cannot win the search.
        if html[cursor..].starts_with("<!--") {
            match html[cursor..].find("-->") {
                Some(end) => cursor += end + 3,
                None => break, // Unterminated comment — bail out of injection.
            }
            continue;
        }
        if html[cursor..].starts_with("<head")
            && let Some(gt) = html[cursor..].find('>')
        {
            let pos = cursor + gt + 1;
            let mut out = String::with_capacity(html.len() + snippet.len());
            out.push_str(&html[..pos]);
            out.push_str(snippet);
            out.push_str(&html[pos..]);
            return out;
        }
        cursor += 1;
    }
    format!("{snippet}{html}")
}

/// Inject `<base href="{href}">` so the served SPA's relative asset/router
/// URLs resolve under the configured prefix.
pub(crate) fn inject_base_href(html: &str, href: &str) -> String {
    inject_into_head(html, &format!("<base href=\"{}\">", escape_attr(href)))
}

/// Inject `<meta name="engram-base-path" content="{base_path}">` so the
/// SPA can build API URLs as `{base_path}/api/v1`. The `<base href>` alone is
/// ambiguous for this because it also folds in the web slug (e.g. `/web`),
/// whereas `/api/v1` hangs off the base path, not the web mount.
pub(crate) fn inject_base_path_meta(html: &str, base_path: &str) -> String {
    inject_into_head(
        html,
        &format!(
            "<meta name=\"engram-base-path\" content=\"{}\">",
            escape_attr(base_path)
        ),
    )
}

#[cfg(test)]
mod web_base_tests {
    use super::{inject_base_href, inject_base_path_meta, normalize_prefix, web_base_href};

    #[test]
    fn normalize_prefix_edge_cases() {
        assert_eq!(normalize_prefix(""), "");
        assert_eq!(normalize_prefix("/"), "");
        assert_eq!(normalize_prefix("//"), "");
        assert_eq!(normalize_prefix("  /  "), "");
        assert_eq!(normalize_prefix("wiki"), "/wiki");
        assert_eq!(normalize_prefix("/wiki"), "/wiki");
        assert_eq!(normalize_prefix("/wiki/"), "/wiki");
        assert_eq!(normalize_prefix("//wiki//"), "/wiki");
        assert_eq!(normalize_prefix("/wiki/sub"), "/wiki/sub");
        // Unsafe chars fall back to root — never inject markup or `//`.
        assert_eq!(normalize_prefix("/wi\"ki"), "");
        assert_eq!(normalize_prefix("/wiki space"), "");
        assert_eq!(normalize_prefix("/<script>"), "");
    }

    /// Dot-segments must NOT survive normalisation. Their characters
    /// pass the unreserved per-char allowlist (`.` is unreserved), so
    /// the segment-level rejection is what stops `/..` and `/.` from
    /// turning the base prefix into a traversal vector. Regression
    /// guard — without this, `ENGRAM_BASE_PATH=/..` would serve
    /// `/..` and let an upstream redirect normalise it to `/`.
    #[test]
    fn normalize_prefix_rejects_dot_segments() {
        assert_eq!(normalize_prefix("/.."), "", "/.. must collapse to root");
        assert_eq!(normalize_prefix("/."), "", "/. must collapse to root");
        assert_eq!(
            normalize_prefix("/wiki/.."),
            "",
            "any embedded /.. fails the whole prefix"
        );
        assert_eq!(
            normalize_prefix("/wiki/./sub"),
            "",
            "any embedded /. fails the whole prefix"
        );
        // Segments that merely START with a dot but aren't pure
        // dot-segments are still valid (RFC 3986 unreserved chars).
        assert_eq!(normalize_prefix("/.wellknown"), "/.wellknown");
    }

    /// Nested base paths (`/a/b/c`) are valid; the normaliser keeps the
    /// hierarchy intact instead of collapsing to one level.
    #[test]
    fn normalize_prefix_keeps_nested_paths() {
        assert_eq!(normalize_prefix("/a/b/c"), "/a/b/c");
        assert_eq!(normalize_prefix("a/b/c"), "/a/b/c");
        assert_eq!(normalize_prefix("//a//b//c//"), "/a/b/c");
        assert_eq!(normalize_prefix("/a/b/c/d/e"), "/a/b/c/d/e");
    }

    /// `inject_into_head` must skip `<head` literals sitting inside an
    /// HTML comment, otherwise a custom SPA whose `index.html` had a
    /// `<!-- <head> placeholder -->` comment would have the snippet
    /// injected at the wrong place.
    #[test]
    fn inject_base_href_skips_head_inside_html_comment() {
        let html =
            "<!-- <head fake --><html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let out = inject_base_href(html, "/w/");
        // Snippet must follow the REAL <head>, not the commented one —
        // the comment must remain unmodified.
        assert!(out.contains("<!-- <head fake -->"));
        assert!(out.contains("<head><base href=\"/w/\"><meta"));
    }

    /// `inject_into_head` falls back to the prepend path on an
    /// unterminated comment instead of looping forever (defensive).
    #[test]
    fn inject_base_href_on_unterminated_comment_falls_back_to_prepend() {
        let html = "<!-- never closes <head>";
        let out = inject_base_href(html, "/w/");
        assert!(out.starts_with("<base href=\"/w/\">"));
    }

    #[test]
    fn web_base_href_never_protocol_relative() {
        assert_eq!(web_base_href("", "/web"), "/web/");
        assert_eq!(web_base_href("/wiki", "/web"), "/wiki/web/");
        assert_eq!(web_base_href("/wiki", "/"), "/wiki/");
        assert_eq!(web_base_href("", "/"), "/");
        assert_eq!(web_base_href("/", "/"), "/");
        assert_eq!(web_base_href("/wiki/", "web"), "/wiki/web/");
        for (b, s) in [("", "/"), ("/", "/"), ("//", "//")] {
            assert!(!web_base_href(b, s).starts_with("//"));
        }
    }

    #[test]
    fn inject_base_href_after_head() {
        let html = "<!doctype html><html><head><meta charset=\"utf-8\"></head><body></body></html>";
        let out = inject_base_href(html, "/wiki/web/");
        assert!(out.contains("<head><base href=\"/wiki/web/\"><meta"));
    }

    #[test]
    fn inject_base_href_no_head_prepends() {
        let out = inject_base_href("<html></html>", "/x/");
        assert!(out.starts_with("<base href=\"/x/\"><html>"));
    }

    #[test]
    fn inject_base_href_escapes_attr() {
        let out = inject_base_href("<head></head>", "/a\"b/");
        assert!(out.contains("<base href=\"/a&quot;b/\">"));
    }

    #[test]
    fn inject_base_path_meta_emits_meta() {
        let out = inject_base_path_meta("<head></head>", "/wiki");
        assert!(out.contains("<meta name=\"engram-base-path\" content=\"/wiki\">"));
        // Empty base path => empty content (SPA falls back to root).
        let empty = inject_base_path_meta("<head></head>", "");
        assert!(empty.contains("content=\"\""));
    }
}

/// Path / URL config the web mount needs. Bundling these together
/// keeps `mount_web_router` and its helpers under clippy's
/// `too_many_arguments` threshold without `#[allow]` papering over
/// the call shape.
pub(crate) struct WebMountSpec<'a> {
    pub web_ui_dir: Option<&'a Path>,
    pub cors_origins: &'a [String],
    pub web_slug: &'a str,
    pub base_href: &'a str,
    pub base_path: &'a str,
}

/// Orchestrator: assemble the `/api/v1` + web-UI surfaces on top of
/// `router`. Skips everything when web is disabled. Each concern lives
/// in a dedicated helper below so this function reads as a four-step
/// recipe (CORS-scoped API, slug normalisation, SPA-vs-builtin choice,
/// final mount).
fn mount_web_router(
    router: axum::Router,
    enable_web: bool,
    reader: ReaderPool,
    wiki: Wiki,
    spec: WebMountSpec<'_>,
) -> Result<axum::Router> {
    if !enable_web {
        return Ok(router);
    }
    // Register the web surfaces BEFORE applying the bearer middleware. In
    // axum 0.8, `.layer()` only attaches to routes registered before the
    // call; nesting after the layer would silently bypass auth for /web/*.
    let router = router.nest(
        "/api/v1",
        build_api_router(&reader, &wiki, spec.cors_origins),
    );

    // Where the UI is mounted WITHIN the (already-applied) base path.
    // Empty slug => the UI is the root of the base path itself.
    let slug = normalize_prefix(spec.web_slug);
    let mount = if slug.is_empty() { "/" } else { slug.as_str() };

    // Custom SPA via --web-ui-dir (SPA fallback to index.html), otherwise
    // the built-in server-side wiki browser. In both cases the served
    // index carries an injected `<base href>` so relative asset/router
    // URLs resolve under `{base_path}{web_slug}`.
    if let Some(dir) = spec.web_ui_dir {
        return mount_custom_spa(router, dir, &slug, spec.base_href, spec.base_path, mount);
    }
    Ok(mount_builtin_browser(
        router,
        reader,
        wiki,
        &slug,
        spec.base_href,
        mount,
    ))
}

/// Build the `/api/v1` router and apply the per-origin CORS layer if
/// the operator configured any. The layer is scoped to this router only
/// (CORS_NOT_APPLIED_TO_OTHER_ROUTES invariant — `/mcp`, `/hook`,
/// `/admin`, and `/web` must remain CORS-free).
fn build_api_router(reader: &ReaderPool, wiki: &Wiki, cors_origins: &[String]) -> axum::Router {
    let api = engram_web::api_router(reader.clone(), wiki.clone());
    if cors_origins.is_empty() {
        return api;
    }
    // Origins were already validated before binding, so parsing here
    // is expected to succeed; `.expect` surfaces a logic bug if it does not.
    let parsed: Vec<axum::http::HeaderValue> = cors_origins
        .iter()
        .map(|o| o.parse().expect("pre-validated origin must parse"))
        .collect();
    let cors = CorsLayer::new()
        .allow_origin(parsed)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE])
        .allow_credentials(true)
        .max_age(Duration::from_secs(600));
    info!(origins = ?cors_origins, "CORS layer attached to /api/v1");
    api.layer(cors)
}

/// Mount the operator's custom SPA from `--web-ui-dir`. Reads
/// `index.html`, injects `<base href>` plus the `engram-base-path`
/// meta, and serves the rest as static assets with an SPA fallback to
/// the injected shell. Errors surface as `anyhow` with the dir path.
fn mount_custom_spa(
    router: axum::Router,
    dir: &Path,
    slug: &str,
    base_href: &str,
    base_path: &str,
    mount: &str,
) -> Result<axum::Router> {
    let dir = dir.to_path_buf();
    let raw = std::fs::read_to_string(dir.join("index.html"))
        .with_context(|| format!("reading custom web UI index at {}", dir.display()))?;
    let injected = inject_base_path_meta(&inject_base_href(&raw, base_href), base_path);
    info!(mount, base_href, base_path, "custom web UI mounted");
    let spa = custom_spa_router(dir, injected.clone());
    Ok(if slug.is_empty() {
        router.merge(spa)
    } else {
        // `nest(slug, …)` routes `{slug}` (→ inner `/`) and `{slug}/<path>`
        // (→ inner `/{*path}`), but NOT the bare trailing-slash root
        // `{slug}/` — that empty sub-path matches neither, so it 404s. The
        // SPA router normalises its home to exactly that URL, so a refresh on
        // the app root returned a hard 404 (`custom_spa_trailing_slash_root_*`).
        // Serve the injected shell there too. Unlike the builtin browser —
        // which redirects `{slug}/` → `{slug}` — a SPA is happier staying put
        // on a 200 than bouncing through a redirect on every root refresh.
        let slash_index = Arc::new(injected);
        router
            .route(
                &format!("{slug}/"),
                axum::routing::get(move || {
                    let body = slash_index.clone();
                    async move { axum::response::Html((*body).clone()) }
                }),
            )
            .nest(slug, spa)
    })
}

/// Mount the built-in server-rendered wiki browser at `slug` with
/// `<base href>` injection middleware. When `slug` is non-empty also
/// register the trailing-slash → canonical redirect, preserving any
/// query string the caller passed.
fn mount_builtin_browser(
    router: axum::Router,
    reader: ReaderPool,
    wiki: Wiki,
    slug: &str,
    base_href: &str,
    mount: &str,
) -> axum::Router {
    // The built-in browser emits RELATIVE asset/link URLs (`static/…`,
    // `w/…`, `search`, `.`). Inject a `<base href>` into every HTML
    // response so they resolve under `{base_path}{web_slug}/` — the
    // same anchoring the custom SPA gets via its injected index.
    let web_router = engram_web::router(reader, wiki).layer(axum::middleware::from_fn_with_state(
        Arc::new(base_href.to_string()),
        inject_web_base_href,
    ));
    info!(mount, base_href, "read-only wiki browser mounted");
    if slug.is_empty() {
        return router.merge(web_router);
    }
    // Strip-trailing-slash redirect. The target must carry the full
    // external prefix because the surrounding `nest(&base_path, …)`
    // does NOT rewrite Location headers. Derive it from base_href
    // (which already folds in base_path + slug). The closure takes
    // `Uri` so `?q=x` survives — see
    // `trailing_slash_redirect_preserves_query_string`.
    let canonical = {
        let trimmed = base_href.trim_end_matches('/');
        if trimmed.is_empty() {
            "/".to_string()
        } else {
            trimmed.to_string()
        }
    };
    router
        .route(
            &format!("{slug}/"),
            axum::routing::get(move |uri: axum::http::Uri| {
                let to = match uri.query() {
                    Some(q) if !q.is_empty() => format!("{canonical}?{q}"),
                    _ => canonical.clone(),
                };
                async move { axum::response::Redirect::permanent(&to) }
            }),
        )
        .nest(slug, web_router)
}

fn custom_spa_router(dir: std::path::PathBuf, injected_index: String) -> axum::Router {
    let index = Arc::new(injected_index);
    let root_index = index.clone();
    let direct_index = index.clone();
    let fallback_index = index.clone();

    // Assets are served as files; any missing asset path falls back to the
    // injected index for SPA client routes. Direct `/index.html` is routed
    // explicitly so it cannot bypass injection by being served from disk.
    let assets = ServeDir::new(dir)
        .append_index_html_on_directories(false)
        .fallback(service_fn(move |_req: Request<Body>| {
            let body = fallback_index.clone();
            async move {
                Ok::<_, Infallible>(axum::response::Html((*body).clone()).into_response())
            }
        }));

    axum::Router::new()
        .route(
            "/",
            axum::routing::get(move || {
                let body = root_index.clone();
                async move { axum::response::Html((*body).clone()) }
            }),
        )
        .route(
            "/index.html",
            axum::routing::get(move || {
                let body = direct_index.clone();
                async move { axum::response::Html((*body).clone()) }
            }),
        )
        .route_service("/{*path}", assets)
}

/// Response middleware: inject `<base href>` into `text/html` responses from
/// the built-in server-rendered web browser, so its relative URLs resolve
/// under the configured `{base_path}{web_slug}` prefix. Non-HTML responses
/// (static assets, redirects) pass through untouched.
async fn inject_web_base_href(
    State(base_href): State<Arc<String>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let resp = next.run(req).await;
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if !is_html {
        return resp;
    }
    let (mut parts, body) = resp.into_parts();
    // Bound the buffer at MAX_BODY_BYTES — same cap inbound bodies use.
    // A custom-SPA `index.html` over the cap is misconfigured at the
    // operator level; refusing here keeps a runaway template (or a
    // hostile asset masquerading as text/html) from streaming
    // unbounded into memory before injection.
    let bytes = match axum::body::to_bytes(body, MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(_) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "response body too large or read error\n",
            )
                .into_response();
        }
    };
    match std::str::from_utf8(&bytes) {
        Ok(html) => {
            let injected = inject_base_href(html, &base_href);
            // Stale length from the pre-injection body; let hyper recompute.
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, Body::from(injected))
        }
        Err(_) => Response::from_parts(parts, Body::from(bytes)),
    }
}

fn apply_http_layers(
    router: axum::Router,
    auth_state: Arc<AuthState>,
    allowed_hosts: Vec<String>,
) -> axum::Router {
    router
        .layer(axum::middleware::from_fn_with_state(
            auth_state,
            require_bearer,
        ))
        .layer(axum::middleware::from_fn_with_state(
            Arc::new(allowed_hosts),
            require_allowed_host,
        ))
}

async fn require_allowed_host(
    State(allowed_hosts): State<Arc<Vec<String>>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
    else {
        return (StatusCode::BAD_REQUEST, "missing Host header\n").into_response();
    };
    if host_allowed(host, &allowed_hosts) {
        return next.run(req).await;
    }
    tracing::warn!(host, allowed = ?allowed_hosts, "rejected request with disallowed Host header");
    (StatusCode::FORBIDDEN, "forbidden host\n").into_response()
}

fn host_allowed(host: &str, allowed_hosts: &[String]) -> bool {
    allowed_hosts.iter().any(|allowed| {
        host.eq_ignore_ascii_case(allowed) || host_without_port(host).eq_ignore_ascii_case(allowed)
    })
}

fn host_without_port(host: &str) -> &str {
    if let Some(rest) = host.strip_prefix('[')
        && let Some((inside, _)) = rest.split_once(']')
    {
        return inside;
    }
    match host.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') && port.chars().all(|c| c.is_ascii_digit()) => {
            name
        }
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use engram_core::{AgentKind, NewSession, PagePath, Tier};
    use engram_llm::{ChatRequest, ChatResponse, LlmResult, SyntheticEmbedder};
    use engram_wiki::WritePageRequest;
    use std::future::Future;
    use std::pin::Pin;
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct PanicLlm;

    impl LlmProvider for PanicLlm {
        fn name(&self) -> &'static str {
            "panic"
        }

        fn model(&self) -> &str {
            "panic"
        }

        fn complete<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
        ) -> Pin<Box<dyn Future<Output = LlmResult<ChatResponse>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }

        fn complete_structured_raw<'life0, 'async_trait>(
            &'life0 self,
            _request: ChatRequest,
            _schema: serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = LlmResult<serde_json::Value>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move { panic!("preflight-skipped scheduler test must not call LLM") })
        }
    }

    #[test]
    fn host_allowed_accepts_host_with_port() {
        let allowed = vec!["127.0.0.1".to_string(), "localhost".to_string()];
        assert!(host_allowed("127.0.0.1:49374", &allowed));
        assert!(host_allowed("localhost", &allowed));
    }

    #[test]
    fn host_allowed_rejects_unknown_host() {
        let allowed = vec!["127.0.0.1".to_string()];
        assert!(!host_allowed("evil.example:49374", &allowed));
    }

    #[test]
    fn host_without_port_handles_ipv6_loopback() {
        assert_eq!(host_without_port("[::1]:49374"), "::1");
    }

    #[test]
    fn web_ui_dir_requires_enable_web() {
        let ui = TempDir::new().unwrap();
        std::fs::write(ui.path().join("index.html"), "custom ui").unwrap();

        let err = validate_web_ui_args(false, Some(ui.path())).unwrap_err();
        assert!(
            err.to_string()
                .contains("--web-ui-dir requires --enable-web"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn web_ui_dir_must_be_directory() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("index.html");
        std::fs::write(&file, "custom ui").unwrap();

        let err = validate_web_ui_args(true, Some(&file)).unwrap_err();
        assert!(
            err.to_string().contains("--web-ui-dir is not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn web_ui_dir_must_include_index_html() {
        let ui = TempDir::new().unwrap();

        let err = validate_web_ui_args(true, Some(ui.path())).unwrap_err();
        assert!(
            err.to_string()
                .contains("--web-ui-dir is missing index.html"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn valid_web_ui_dir_passes_validation() {
        let ui = TempDir::new().unwrap();
        std::fs::write(ui.path().join("index.html"), "custom ui").unwrap();

        validate_web_ui_args(true, Some(ui.path())).unwrap();
    }

    #[tokio::test]
    async fn web_routes_are_inside_auth_layer() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &[],
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        let router = apply_http_layers(
            router,
            Arc::new(AuthState::new(Some("secret".to_string()))),
            vec!["localhost".to_string()],
        );

        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    /// Assemble the web/API surface under `base_path` + `web_slug` exactly the
    /// way the `serve` handler does (mount, then `nest(&base_path, …)`), with
    /// no auth/host layers so tests probe routing + injection in isolation.
    /// Returns the `TempDir` guard too — the caller must keep it alive for the
    /// router's lifetime (the store's SQLite + wiki files live under it).
    fn based_web_router(base_path: &str, web_slug: &str) -> (TempDir, axum::Router) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let base = normalize_prefix(base_path);
        let base_href = web_base_href(base_path, web_slug);
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &[],
                web_slug,
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = if base.is_empty() {
            router
        } else {
            axum::Router::new().nest(&base, router)
        };
        // Mirror the production favicon mount in serve handler: at the
        // absolute host root, outside the base-path nest.
        let router = router.merge(engram_web::favicon_router());
        (tmp, router)
    }

    #[tokio::test]
    async fn base_path_nests_all_surfaces_and_root_404s() {
        let (_tmp, router) = based_web_router("/wiki", "/web");

        // The web UI is reachable UNDER the prefix…
        let under = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(under.status(), StatusCode::OK);

        // …and the API too.
        let api = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::OK);

        // The same paths at the host ROOT must 404 — nothing leaks outside the
        // prefix (the whole point of base-path hosting behind a shared proxy).
        for uri in ["/web", "/api/v1/projects"] {
            let root = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                root.status(),
                StatusCode::NOT_FOUND,
                "{uri} must 404 at root"
            );
        }
    }

    #[tokio::test]
    async fn inject_web_base_href_targets_html_only() {
        let (_tmp, router) = based_web_router("/wiki", "/web");

        // HTML response carries the injected <base href> under the prefix.
        let html_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(html_resp.status(), StatusCode::OK);
        let html = axum::body::to_bytes(html_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = std::str::from_utf8(&html).unwrap();
        assert!(
            html.contains(r#"<base href="/wiki/web/">"#),
            "expected injected base href, got: {html}"
        );

        // A non-HTML asset passes through untouched (no <base> smuggled in).
        let css_resp = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/static/tailwind.css")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(css_resp.status(), StatusCode::OK);
        let css = axum::body::to_bytes(css_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            !std::str::from_utf8(&css).unwrap().contains("<base href"),
            "non-HTML asset must not receive a <base href> injection"
        );
    }

    #[tokio::test]
    async fn trailing_slash_redirect_carries_the_prefix() {
        let (_tmp, router) = based_web_router("/wiki", "/web");
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        // Location must include the external prefix — the surrounding base nest
        // does NOT rewrite Location headers, so a bare `/web` would drop `/wiki`.
        assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/wiki/web",);
    }

    /// Trailing-slash redirect must preserve the query string. The
    /// original handler took `()` and silently dropped `?q=x`, so a
    /// link in the SPA that appended a filter param round-tripped to
    /// the canonical URL with the param missing. Fragments are
    /// client-only and never reach the server, so we only assert query.
    #[tokio::test]
    async fn trailing_slash_redirect_preserves_query_string() {
        let (_tmp, router) = based_web_router("/wiki", "/web");
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/?q=foo&limit=5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            resp.headers().get(header::LOCATION).unwrap(),
            "/wiki/web?q=foo&limit=5",
            "redirect must carry the original query, not drop it"
        );
    }

    /// Nested base paths (`/a/b/c`) — exercised by the normaliser
    /// unit test but not previously end-to-end. Routes must be
    /// reachable under the full prefix and 404 at any shorter prefix.
    #[tokio::test]
    async fn nested_base_path_nests_web_and_api() {
        let (_tmp, router) = based_web_router("/a/b/c", "/web");
        // Full nested prefix reaches both surfaces.
        for uri in ["/a/b/c/web", "/a/b/c/api/v1/projects"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} must reach the surface"
            );
        }
        // A SHORTER prefix (e.g. only /a/b) must NOT leak — the nest is
        // exactly `/a/b/c` and any partial mount is unmapped.
        for uri in ["/a/b/web", "/a/api/v1/projects"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "{uri} must 404 — leaks the prefix otherwise"
            );
        }
    }

    /// Custom-SPA index that already carries its own `<base href>` —
    /// today we prepend another one; HTML5 says the browser honours
    /// the FIRST `<base>` so the injection becomes a silent no-op.
    /// Documented behaviour (see `inject_into_head` doc-comment) but
    /// also exercised here so a future change to "replace existing"
    /// gets noticed by a failing test rather than silently shipping.
    #[test]
    fn inject_base_href_with_existing_base_tag_does_not_replace() {
        let html = "<html><head><base href=\"/old/\"><title>x</title></head></html>";
        let out = inject_base_href(html, "/new/");
        // Injected snippet appears first, the old <base> remains too.
        let new_pos = out.find("<base href=\"/new/\">").expect("new injected");
        let old_pos = out.find("<base href=\"/old/\">").expect("old preserved");
        assert!(
            new_pos < old_pos,
            "injected base must appear before the pre-existing one (browser ignores duplicates after the first)"
        );
    }

    /// Post-merge audit (Phase 7 live test) caught that PR #79's
    /// `/favicon.ico` route was nested inside `/web`, so it lived at
    /// `/web/favicon.ico` and the browser's automatic root fetch always
    /// 404'd. Fix: a separate `favicon_router()` mounted at the absolute
    /// HOST root, outside `--base-path` and outside the `/web` nest.
    /// This test pins both:
    ///   * `/favicon.ico` at the host root returns the PNG.
    ///   * Under `--base-path /wiki`, the favicon STAYS at root —
    ///     browsers fetch `<host>/favicon.ico` regardless of where the
    ///     app is mounted; the route must not move with the prefix.
    #[tokio::test]
    async fn favicon_lives_at_host_root_regardless_of_base_path() {
        for base_path in ["", "/wiki"] {
            let (_tmp, router) = based_web_router(base_path, "/web");
            let resp = router
                .oneshot(
                    Request::builder()
                        .uri("/favicon.ico")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "/favicon.ico must be reachable at host root (base_path={base_path:?})"
            );
            assert_eq!(
                resp.headers()
                    .get(axum::http::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok()),
                Some("image/png"),
            );
        }
    }

    #[tokio::test]
    async fn no_base_path_is_byte_equivalent_at_root() {
        let (_tmp, router) = based_web_router("", "/web");
        let resp = router
            .clone()
            .oneshot(Request::builder().uri("/web").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            std::str::from_utf8(&body)
                .unwrap()
                .contains(r#"<base href="/web/">"#),
            "default mount should inject the root-relative base href"
        );
    }

    #[tokio::test]
    async fn custom_spa_index_routes_are_injected_under_base_path() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>shell</body></html>",
        )
        .unwrap();
        std::fs::write(ui.path().join("app.js"), "console.log('asset');").unwrap();

        let base = normalize_prefix("/wiki");
        let base_href = web_base_href("/wiki", "/web");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/web",
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = axum::Router::new().nest(&base, router);

        for uri in [
            "/wiki/web",
            "/wiki/web/", // trailing-slash root: SPA home after router normalises it
            "/wiki/web/index.html",
            "/wiki/web/client/route",
        ] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains(r#"<base href="/wiki/web/">"#),
                "{uri} must receive injected base href: {html}"
            );
            assert!(
                html.contains(r#"<meta name="engram-base-path" content="/wiki">"#),
                "{uri} must receive injected API base-path meta: {html}"
            );
            assert!(html.contains("shell"), "{uri} returns the SPA shell");
        }

        let asset = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/web/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        let body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).unwrap();
        assert_eq!(js, "console.log('asset');");
    }

    /// Regression for the prod 404: with `base_path=""` (host root) the custom
    /// SPA mounts at slug `/web`. `nest("/web", …)` served `/web` and
    /// `/web/<route>` but left the bare trailing-slash root `/web/` unrouted →
    /// hard 404. The SPA normalises its home to exactly `/web/`, so refreshing
    /// the app root broke (both a host-root deploy and one mounted under a base
    /// path like `/wiki`).
    #[tokio::test]
    async fn custom_spa_trailing_slash_root_serves_shell() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>shell</body></html>",
        )
        .unwrap();

        let base_href = web_base_href("", "/web");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/web",
                base_href: &base_href,
                base_path: "",
            },
        )
        .unwrap();

        // `/web`, the trailing-slash root `/web/`, and a deep client route all
        // serve the injected shell — none may 404.
        for uri in ["/web", "/web/", "/web/projects/x"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::OK,
                "{uri} must serve the SPA shell, not 404"
            );
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains("shell"),
                "{uri} returns the SPA shell: {html}"
            );
            assert!(
                html.contains(r#"<base href="/web/">"#),
                "{uri} must receive the injected base href: {html}"
            );
        }
    }

    #[tokio::test]
    async fn custom_spa_root_slug_does_not_shadow_api_routes() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ui = TempDir::new().unwrap();
        std::fs::write(
            ui.path().join("index.html"),
            "<!doctype html><html><head><title>spa</title></head><body>root shell</body></html>",
        )
        .unwrap();
        std::fs::write(ui.path().join("app.js"), "console.log('root asset');").unwrap();

        let base = normalize_prefix("/wiki");
        let base_href = web_base_href("/wiki", "/");
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: Some(ui.path()),
                cors_origins: &[],
                web_slug: "/",
                base_href: &base_href,
                base_path: &base,
            },
        )
        .unwrap();
        let router = axum::Router::new().nest(&base, router);

        for uri in ["/wiki", "/wiki/index.html", "/wiki/client/route"] {
            let resp = router
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri}");
            let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            assert!(
                html.contains(r#"<base href="/wiki/">"#),
                "{uri} must receive injected base href: {html}"
            );
            assert!(
                html.contains(r#"<meta name="engram-base-path" content="/wiki">"#),
                "{uri} must receive injected API base-path meta: {html}"
            );
            assert!(html.contains("root shell"), "{uri} returns the SPA shell");
        }

        let api = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/wiki/api/v1/projects")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::OK);
        let content_type = api
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("application/json"),
            "API route must not be shadowed by the root SPA: {content_type}"
        );

        let asset = router
            .oneshot(
                Request::builder()
                    .uri("/wiki/app.js")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(asset.status(), StatusCode::OK);
        let body = axum::body::to_bytes(asset.into_body(), usize::MAX)
            .await
            .unwrap();
        let js = std::str::from_utf8(&body).unwrap();
        assert_eq!(js, "console.log('root asset');");
    }

    #[tokio::test]
    async fn embedder_mismatch_warns_but_keeps_server_startable() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let proj = store
            .writer
            .get_or_create_project(ws, "scratch", None)
            .await
            .unwrap();

        let synthetic: Arc<dyn Embedder> = Arc::new(SyntheticEmbedder::new(64));
        let wiki = Wiki::new(tmp.path(), store.writer.clone())
            .unwrap()
            .with_embedder(synthetic);
        wiki.write_page(WritePageRequest {
            workspace_id: ws,
            project_id: proj,
            path: PagePath::new("notes/old-embedding.md").unwrap(),
            frontmatter: serde_json::json!({"title": "old embedding"}),
            body: "existing vector row".into(),
            tier: Tier::Semantic,
            pinned: false,
            title: None,
            admission_ctx: None,
            author_id: None,
            actor: engram_core::ActorContext::anonymous(),
        })
        .await
        .unwrap();

        let cfg = Config {
            data_dir: tmp.path().to_path_buf(),
            embedding_provider: Some("openai".into()),
            runtime_env: crate::config::RuntimeEnv::with_openai_api_key_for_tests("test-key"),
            ..Config::default()
        };
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let provider_health = ProviderHealth::default();
        let (_wiki, embedder) = configure_embedder(&cfg, &store, wiki, &provider_health)
            .await
            .unwrap();

        let embedder = embedder.expect("configured embedder should be enabled");
        assert_eq!(embedder.provider(), "openai");
        assert_eq!(
            provider_health.snapshot().embedding.status,
            engram_llm::ProviderHealthStatus::Unknown
        );
    }

    #[tokio::test]
    async fn auto_improve_scheduler_startup_init_preserves_first_interval_sessions() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        let ws = store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();
        let first_project = store
            .writer
            .get_or_create_project(ws, "first", None)
            .await
            .unwrap();
        let second_project = store
            .writer
            .get_or_create_project(ws, "second", None)
            .await
            .unwrap();

        for project_id in [first_project, second_project] {
            let before_startup_init = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: before_startup_init,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
            store
                .writer
                .end_session(before_startup_init, None)
                .await
                .unwrap();
        }

        assert_eq!(
            initialize_auto_improve_scheduler_scopes(&store.reader, &store.writer)
                .await
                .unwrap(),
            (2, 0)
        );

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        let mut first_interval_sessions = Vec::new();
        for project_id in [first_project, second_project] {
            let session_id = SessionId::new();
            store
                .writer
                .begin_session(NewSession {
                    id: session_id,
                    workspace_id: ws,
                    project_id,
                    agent_kind: AgentKind::OpenCode,
                    cwd: None,
                })
                .await
                .unwrap();
            store.writer.end_session(session_id, None).await.unwrap();
            first_interval_sessions.push((project_id, session_id));
        }

        let mut settings = AutoImproveSettings::default();
        settings.scheduler.min_session_age_secs = 0;
        settings.scheduler.max_sessions_per_tick = 10;
        let llm: Arc<dyn LlmProvider> = Arc::new(PanicLlm);
        let outcome =
            run_auto_improve_scheduler_tick(&store.reader, &store.writer, &wiki, &llm, &settings)
                .await
                .unwrap();

        assert_eq!(outcome.scopes, 2);
        assert_eq!(outcome.scopes_with_candidates, 2);
        assert_eq!(outcome.reviewed, 4);
        assert_eq!(outcome.errors, 0);

        for (project_id, session_id) in first_interval_sessions {
            let candidates = store
                .reader
                .auto_improve_candidate_sessions(ws, project_id, 0, 10)
                .await
                .unwrap();
            assert!(
                candidates.iter().all(|c| c.session_id != session_id),
                "first-interval session should have been reviewed or claimed"
            );
        }
    }

    // ── Part B: CORS validation tests ──────────────────────────────────────

    #[test]
    fn validate_cors_origins_rejects_wildcard() {
        let err = validate_cors_origins(&["*".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("wildcard"),
            "error must mention wildcard: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_rejects_missing_scheme() {
        let err = validate_cors_origins(&["app.example.com".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("missing a scheme"),
            "error must mention missing scheme: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_rejects_trailing_slash() {
        let err = validate_cors_origins(&["https://app.example.com/".to_string()]).unwrap_err();
        assert!(
            err.to_string().contains("trailing slash"),
            "error must mention trailing slash: {err}"
        );
    }

    #[test]
    fn validate_cors_origins_accepts_well_formed() {
        validate_cors_origins(&[
            "https://app.example.com".to_string(),
            "http://localhost:5173".to_string(),
        ])
        .unwrap();
    }

    #[test]
    fn validate_cors_origins_accepts_empty_list() {
        validate_cors_origins(&[]).unwrap();
    }

    #[test]
    fn merge_cors_origins_deduplicates_preserving_order() {
        let merged = merge_cors_origins(
            &[
                "https://a.example.com".to_string(),
                "https://b.example.com".to_string(),
            ],
            &[
                "https://b.example.com".to_string(),
                "https://c.example.com".to_string(),
            ],
        );
        assert_eq!(
            merged,
            vec![
                "https://a.example.com",
                "https://b.example.com",
                "https://c.example.com"
            ]
        );
    }

    #[tokio::test]
    async fn cors_layer_on_api_v1_allows_configured_origin() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        // No auth layer so we can reach /api/v1 directly.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/workspaces")
                    .header("Origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let acao = resp
            .headers()
            .get("access-control-allow-origin")
            .expect("ACAO header must be present for allowed origin")
            .to_str()
            .unwrap();
        assert_eq!(acao, "https://app.example.com");
        let acac = resp
            .headers()
            .get("access-control-allow-credentials")
            .expect("ACAC header must be present")
            .to_str()
            .unwrap();
        assert_eq!(acac, "true");
    }

    #[tokio::test]
    async fn cors_layer_on_api_v1_denies_unlisted_origin() {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();
        store
            .writer
            .get_or_create_workspace("default")
            .await
            .unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/api/v1/workspaces")
                    .header("Origin", "https://evil.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // The request is still served (CORS does not block on the server side),
        // but the ACAO header must be absent so the browser enforces the policy.
        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "unlisted origin must not receive ACAO header"
        );
    }

    #[tokio::test]
    async fn cors_not_applied_to_other_routes() {
        // /mcp and /admin routes must not carry CORS headers even when
        // a CORS origin list is configured (CORS_NOT_APPLIED_TO_OTHER_ROUTES
        // invariant). We verify by checking that a request to a non-/api/v1
        // path that 404s (no actual handler mounted here) still lacks ACAO.
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path()).unwrap();
        let wiki = Wiki::new(tmp.path(), store.writer.clone()).unwrap();

        let cors_origins = ["https://app.example.com".to_string()];
        let router = mount_web_router(
            axum::Router::new(),
            true,
            store.reader.clone(),
            wiki,
            WebMountSpec {
                web_ui_dir: None,
                cors_origins: &cors_origins,
                web_slug: "/web",
                base_href: "/web/",
                base_path: "",
            },
        )
        .unwrap();
        // /web is a non-api route; sending an Origin header must not trigger CORS.
        let resp = router
            .oneshot(
                Request::builder()
                    .uri("/web")
                    .header("Origin", "https://app.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            resp.headers().get("access-control-allow-origin").is_none(),
            "/web must not carry CORS headers: {:?}",
            resp.headers()
        );
    }
}
