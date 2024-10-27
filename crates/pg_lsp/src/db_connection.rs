use pg_commands::ExecuteStatementCommand;
use pg_schema_cache::SchemaCache;
use sqlx::{
    postgres::{PgListener, PgQueryResult},
    PgPool,
};
use tokio::task::JoinHandle;

#[derive(Debug)]
pub(crate) struct DbConnection {
    pool: PgPool,
    connection_string: String,
    schema_update_handle: Option<JoinHandle<()>>,
}

impl DbConnection {
    pub(crate) async fn new(connection_string: String) -> Result<Self, sqlx::Error> {
        let pool = PgPool::connect(&connection_string).await?;
        Ok(Self {
            pool,
            connection_string: connection_string,
            schema_update_handle: None,
        })
    }

    /// TODO: this should simply take a `Command` type, and the individual
    /// enums should have their deps included (i.e. `ExecuteStatement(String)`)
    pub async fn run_stmt(&self, stmt: String) -> anyhow::Result<PgQueryResult> {
        let command = ExecuteStatementCommand::new(stmt);
        let pool = self.pool.clone();
        command.run(Some(pool)).await
    }

    pub(crate) fn connected_to(&self, connection_string: &str) -> bool {
        connection_string == self.connection_string
    }

    pub(crate) async fn close(self) {
        if self.schema_update_handle.is_some() {
            self.schema_update_handle.unwrap().abort();
        }
        self.pool.close().await;
    }

    pub(crate) async fn listen_for_schema_updates<F>(
        &mut self,
        on_schema_update: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(SchemaCache) -> () + Send + 'static,
    {
        let mut listener = PgListener::connect_with(&self.pool).await?;
        listener.listen_all(["postgres_lsp", "pgrst"]).await?;

        let pool = self.pool.clone();

        let handle: JoinHandle<()> = tokio::spawn(async move {
            loop {
                match listener.recv().await {
                    Ok(not) => {
                        if not.payload().to_string() == "reload schema" {
                            let schema_cache = SchemaCache::load(&pool).await;
                            on_schema_update(schema_cache);
                        };
                    }
                    Err(why) => {
                        eprintln!("Error receiving notification: {:?}", why);
                        break;
                    }
                }
            }
        });

        self.schema_update_handle = Some(handle);

        Ok(())
    }

    pub(crate) fn get_pool(&self) -> PgPool {
        self.pool.clone()
    }
}
