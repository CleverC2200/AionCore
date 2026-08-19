use aionui_db::{IAgentMetadataRepository, SqliteAgentMetadataRepository, init_database_memory};

#[tokio::test]
async fn builtin_aionrs_agent_uses_the_gea_cli_name() {
    let db = init_database_memory().await.unwrap();
    let repo = SqliteAgentMetadataRepository::new(db.pool().clone());

    let aionrs = repo.get("632f31d2").await.unwrap().expect("seeded GEA CLI row");

    assert_eq!(aionrs.name, "GEA CLI");
    assert_eq!(aionrs.agent_type, "aionrs");
    assert_eq!(aionrs.agent_source, "internal");
}
