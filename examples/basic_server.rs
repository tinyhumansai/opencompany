use opencompany::{AppConfig, AppState};

#[tokio::main]
async fn main() -> opencompany::Result<()> {
    let state = AppState::new(AppConfig::default());
    println!("{}", serde_json::to_string_pretty(&state.spec()).unwrap());
    Ok(())
}
