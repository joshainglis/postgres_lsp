use notification::ShowMessage;
use pg_commands::CommandType;
use tokio::sync::RwLock;
use tower_lsp::jsonrpc;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer};

use crate::client::client_flags::ClientFlags;
use crate::server::options::ClientConfigurationOptions;
use crate::session::Session;
use crate::utils::file_path;
use crate::utils::normalize_uri;
use crate::utils::to_proto;

struct Server {
    client: Client,
    session: Session,
    client_capabilities: RwLock<Option<ClientFlags>>,
}

impl Server {
    pub async fn new(client: Client) -> Self {
        Self {
            client,
            session: Session::new(),
            client_capabilities: RwLock::new(None),
        }
    }

    /// When the client sends a didChangeConfiguration notification, we need to parse the received JSON.
    fn parse_options_from_client(
        &self,
        mut value: serde_json::Value,
    ) -> Option<ClientConfigurationOptions> {
        let options = match value.get_mut("pglsp") {
            Some(section) => section.take(),
            None => value,
        };

        match serde_json::from_value::<ClientConfigurationOptions>(options) {
            Ok(new_options) => Some(new_options),
            Err(why) => {
                let message = format!(
                    "The texlab configuration is invalid; using the default settings instead.\nDetails: {why}"
                );
                let typ = MessageType::WARNING;
                self.client
                    .send_notification::<ShowMessage>(ShowMessageParams { message, typ });
                None
            }
        }
    }

    async fn request_opts_from_client(&self) -> Option<ClientConfigurationOptions> {
        let params = ConfigurationParams {
            items: vec![ConfigurationItem {
                section: Some("pglsp".to_string()),
                scope_uri: None,
            }],
        };

        match self
            .client
            .send_request::<request::WorkspaceConfiguration>(params)
            .await
        {
            Ok(json) => {
                // The client reponse fits the requested `ConfigurationParams.items`,
                // so the first value is what we're looking for.
                let relevant = json
                    .into_iter()
                    .next()
                    .expect("workspace/configuration request did not yield expected response.");

                let opts = self.parse_options_from_client(relevant);

                opts
            }
            Err(why) => {
                let message = format!(
                    "Unable to pull client options via workspace/configuration request: {}",
                    why
                );
                println!("{}", message);
                self.client.log_message(MessageType::ERROR, message);
                None
            }
        }
    }

    async fn publish_diagnostics(&self, mut uri: Url) {
        normalize_uri(&mut uri);

        let url = file_path(&uri);
        let diagnostics = self.session.get_diagnostics(url).await;

        let diagnostics: Vec<Diagnostic> = diagnostics
            .into_iter()
            .map(|(d, r)| to_proto::diagnostic(d, r))
            .collect();

        self.client
            .send_notification::<ShowMessage>(ShowMessageParams {
                typ: MessageType::INFO,
                message: format!("diagnostics {}", diagnostics.len()),
            });

        let params = PublishDiagnosticsParams {
            uri,
            diagnostics,
            version: None,
        };

        self.client
            .send_notification::<notification::PublishDiagnostics>(params);
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Server {
    async fn initialize(&self, params: InitializeParams) -> jsonrpc::Result<InitializeResult> {
        let flags = ClientFlags::from_initialize_request_params(&params);
        self.client_capabilities.blocking_write().replace(flags);

        Ok(InitializeResult {
            server_info: None,
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: CommandType::ALL
                        .iter()
                        .map(|c| c.id().to_string())
                        .collect(),
                    ..Default::default()
                }),
                inlay_hint_provider: Some(OneOf::Left(true)),
                code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions::default()),
                ..ServerCapabilities::default()
            },
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "Postgres LSP Connected!")
            .await;
    }

    async fn shutdown(&self) -> jsonrpc::Result<()> {
        self.client
            .log_message(MessageType::INFO, "Postgres LSP terminated.")
            .await;
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        let capabilities = self.client_capabilities.read().await;

        if capabilities.as_ref().unwrap().supports_pull_opts {
            let opts = self.request_opts_from_client().await;
            if opts
                .as_ref()
                .is_some_and(|o| o.db_connection_string.is_some())
            {
                let conn_str = opts.unwrap().db_connection_string.unwrap();
                self.session.change_db(conn_str).await;
                return;
            }
        }

        // if we couldn't pull settings from the client,
        // we'll try parsing the passed in params.
        let opts = self.parse_options_from_client(params.settings);

        if opts
            .as_ref()
            .is_some_and(|o| o.db_connection_string.is_some())
        {
            let conn_str = opts.unwrap().db_connection_string.unwrap();
            self.session.change_db(conn_str).await;
        }
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let mut uri = params.text_document.uri;
        normalize_uri(&mut uri);

        let changed_urls = self
            .session
            .apply_doc_changes(
                file_path(url),
                params.text_document.version,
                params.text_document.text,
            )
            .await;

        for url in changed_urls {
            self.publish_diagnostics(url).await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let mut uri = params.text_document.uri;
        normalize_uri(&mut uri);

        self.publish_diagnostics(uri).await;

        // TODO: "Compute Now"
        let changed_urls = self.session.recompute_and_get_changed_files();
        for url in changed_urls {
            self.publish_diagnostics(url).await;
        }
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        todo!()
    }

    async fn did_close(&self, params: DidSaveTextDocumentParams) {
        let mut uri = params.text_document.uri;
        normalize_uri(&mut uri);
        let path = file_path(&uri);

        self.session.on_file_closed(path);
    }

    async fn code_action(&self, params: CodeActionParams) -> Result<Option<CodeActionResponse>> {
        let mut uri = params.text_document.uri;
        normalize_uri(&mut uri);

        let path = file_path(&uri);
        let range = params.range;

        let actions = self.session.get_available_code_actions(path, range);

        Ok(actions)
    }

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> jsonrpc::Result<Option<serde_json::Value>> {
        match CommandType::from_id(params.command.replace("pglsp.", "").as_str()) {
            Some(CommandType::ExecuteStatement) => {
                if params.arguments.is_empty() {
                    return jsonrpc::Result::Err(jsonrpc::Error::invalid_request());
                }

                let params = params.arguments.into_iter().next().unwrap();
                let stmt = serde_json::from_value(params)
                    .map_err(|_| jsonrpc::Error::invalid_request())?;

                match self.session.run_stmt(stmt).await {
                    Ok(rows_affected) => {
                        self.client
                            .send_notification::<ShowMessage>(ShowMessageParams {
                                typ: MessageType::INFO,
                                message: format!("Success! Affected rows: {}", rows_affected),
                            })
                            .await;
                    }
                    Err(why) => {
                        self.client
                            .send_notification::<ShowMessage>(ShowMessageParams {
                                typ: MessageType::ERROR,
                                message: format!("Error! Statement exectuion failed: {}", why),
                            })
                            .await;
                    }
                };
            }
            None => {
                self.client
                    .show_message(
                        MessageType::ERROR,
                        format!("Unknown command: {}", params.command),
                    )
                    .await;
            }
        };

        Ok(None)
    }
}
