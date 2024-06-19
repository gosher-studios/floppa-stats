use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use std::fs::{self, File};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::time::{interval, Duration};
use tokio::sync::RwLock;
use axum::Router;
use axum::routing::get;
use axum::extract::State;
use askama::Template;
use tower_http::services::ServeDir;
use reqwest::Client;
use tracing::{error, info, info_span, Instrument};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::FmtSpan;
use serde::Deserialize;

pub type Result<T = (), E = Box<dyn std::error::Error>> = std::result::Result<T, E>;

#[tokio::main]
async fn main() -> Result {
  tracing_subscriber::registry()
    //.with(console_subscriber::spawn())
    .with(
      tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_filter(LevelFilter::DEBUG),
    )
    .init();

  let state = match Config::load("config.toml") {
    Ok(c) => Arc::new(AppState::from(c)),
    Err(e) => {
      error!("could not load config: {}", e);
      return Ok(());
    }
  };

  tokio::spawn(process_stats(state.clone()));

  let app = Router::new()
    .route("/", get(home))
    .nest_service("/static", ServeDir::new("static"))
    .with_state(state.clone());
  let listener = TcpListener::bind(state.config.listen).await?;
  info!("listening on http://{}", listener.local_addr()?);
  axum::serve(listener, app).await?;
  Ok(())
}

#[derive(Deserialize)]
struct Config {
  stats_dir: PathBuf,
  refresh_interval: u64,
  listen: SocketAddr,
  players: Vec<String>,
}

impl Config {
  fn load(path: &str) -> Result<Self> {
    Ok(toml::from_str(&fs::read_to_string(path)?)?)
  }
}

struct AppState {
  config: Config,
  players: RwLock<HashMap<String, Option<Player>>>,
}

impl AppState {
  fn from(config: Config) -> Self {
    Self {
      players: RwLock::new(HashMap::from_iter(
        config.players.clone().into_iter().map(|u| (u, None)),
      )),
      config,
    }
  }
}

#[derive(Clone, Debug)]
struct Player {
  username: String,
  mined: u64,
  distance: String,
  jumps: u64,
  kills: u64,
  crafted: u64,
  trades: u64,
  deaths: u64,
}

#[derive(Deserialize)]
struct StatFile {
  stats: HashMap<String, HashMap<String, u64>>,
}

impl StatFile {
  fn sum(&self, key: &str) -> u64 {
    self
      .stats
      .get(key)
      .map(|h| h.values().sum())
      .unwrap_or_default()
  }

  fn custom(&self, key: &str) -> u64 {
    self
      .stats
      .get("minecraft:custom")
      .and_then(|h| h.get(key).cloned())
      .unwrap_or_default()
  }
}

async fn process_stats(state: Arc<AppState>) {
  let mut interval = interval(Duration::from_secs(state.config.refresh_interval));
  let client = Client::new();
  loop {
    interval.tick().await;
    async {
      let mut players = state.players.read().await.clone();
      for (u, p) in players.iter_mut() {
        let username = match p {
          Some(p) => p.username.clone(),
          None => match get_profile(&client, u).await {
            Ok(p) => p.name,
            Err(e) => {
              error!("mojang api error: {}", e);
              continue;
            }
          },
        };

        let filename = state.config.stats_dir.join(u).with_extension("json");
        match File::open(&filename) {
          Ok(f) => match serde_json::from_reader::<_, StatFile>(f) {
            Ok(data) => {
              *p = Some(Player {
                username,
                mined: data.sum("minecraft:mined"),
                distance: format!(
                  "{:.2}km",
                  data
                    .stats
                    .get("minecraft:custom")
                    .map(|h| {
                      h.iter()
                        .filter(|x| x.0.ends_with("cm"))
                        .map(|x| x.1)
                        .sum::<u64>()
                    })
                    .unwrap_or_default() as f32
                    / 100000.0
                ),
                jumps: data.custom("minecraft:jump"),
                kills: data.custom("minecraft:mob_kills") + data.custom("minecraft:player_kills"),
                crafted: data.sum("minecraft:crafted"),
                trades: data.custom("minecraft:traded_with_villager"),
                deaths: data.custom("minecraft:deaths"),
              });
            }
            Err(e) => error!("could not parse {:?}: {}", filename, e),
          },
          Err(e) => error!("could not open {:?}: {}", filename, e),
        }
      }
      *state.players.write().await = players;
    }
    .instrument(info_span!("refreshing stats"))
    .await;
  }
}

#[derive(Deserialize)]
struct Profile {
  name: String,
}

async fn get_profile(client: &Client, uuid: &str) -> Result<Profile> {
  Ok(
    client
      .get(format!("https://api.mojang.com/user/profile/{}", uuid))
      .send()
      .await?
      .json::<Profile>()
      .await?,
  )
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
  players: HashMap<String, Player>,
  ver: &'static str,
}

async fn home<'a>(State(state): State<Arc<AppState>>) -> Home {
  Home {
    players: state
      .players
      .read()
      .await
      .clone()
      .into_iter()
      .filter_map(|(u, p)| p.map(|p| (u, p)))
      .collect(),
    ver: env!("CARGO_PKG_VERSION"),
  }
}
