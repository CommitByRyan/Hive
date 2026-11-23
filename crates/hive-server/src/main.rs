use hive_config::Config;
use hive_llm::ModelCatalog;
use hive_server::{build_router, AppState, TICK_PERIOD};
use hive_store::Store;

#[tokio::main]
async fn main() {
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("configuration error: {error}");
            std::process::exit(1);
        }
    };

    std::fs::create_dir_all(config.data_dir()).ok();
    let db_path = config.data_dir().join("hive.db");
    let mut store = match Store::open(&db_path) {
        Ok(store) => store,
        Err(error) => {
            eprintln!("failed to open store at {}: {error}", db_path.display());
            std::process::exit(1);
        }
    };

    if let Err(error) = hive_runtime::run_startup(&mut store, hive_store::now_ms()) {
        eprintln!("startup recovery failed: {error}");
        std::process::exit(1);
    }

    let mode = config.mode().to_string();
    let host = config.host().to_owned();
    let port = config.port();
    let state = AppState::new(store, ModelCatalog::new(), mode, host.clone(), port);
    state.start_tick_loop(TICK_PERIOD);
    let bind_addr = state.bind_addr();
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("failed to bind {bind_addr}: {error}");
            std::process::exit(1);
        }
    };

    let app = build_router(state);
    eprintln!("hive-server listening on http://{bind_addr}");
    if let Err(error) = axum::serve(listener, app).await {
        eprintln!("server error: {error}");
        std::process::exit(1);
    }
}
