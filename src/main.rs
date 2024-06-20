use std::sync::Arc;
use std::path::PathBuf;
use std::collections::HashMap;
use std::cmp::Ordering;
use std::fs::{self, File};
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::time::{interval, Duration};
use tokio::sync::RwLock;
use axum::Router;
use axum::routing::get;
use axum::extract::{State, Query};
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
  playtime: f32,
  mined: u64,
  distance: f32,
  jumps: u64,
  kills: u64,
  crafted: u64,
  trades: u64,
  deaths: u64,
}

impl Player {
  fn cmp(&self, other: &Player, key: &str) -> Ordering {
    match key {
      "playtime" => other.playtime.total_cmp(&self.playtime),
      "mined" => other.mined.cmp(&self.mined),
      "distance" => other.distance.total_cmp(&self.distance),
      "jumps" => other.jumps.cmp(&self.jumps),
      "kills" => other.kills.cmp(&self.kills),
      "crafted" => other.crafted.cmp(&self.crafted),
      "trades" => other.trades.cmp(&self.trades),
      "deaths" => other.deaths.cmp(&self.deaths),
      _ => self.username.cmp(&other.username),
    }
  }
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
                playtime: data.custom("minecraft:play_time") as f32 / 72000.0,
                mined: data.sum("minecraft:mined"),
                distance: data
                  .stats
                  .get("minecraft:custom")
                  .map(|h| {
                    h.iter()
                      .filter(|x| x.0.ends_with("cm"))
                      .map(|x| x.1)
                      .sum::<u64>()
                  })
                  .unwrap_or_default() as f32
                  / 100000.0,
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

#[derive(Deserialize)]
struct Params {
  sort: Option<String>,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
  players: Vec<(String, Player)>,
  sort_mode: String,
  ver: &'static str,
}

async fn home<'a>(State(state): State<Arc<AppState>>, Query(params): Query<Params>) -> Home {
  let sort_mode = params.sort.unwrap_or("default".to_string());
  let mut players: Vec<_> = state
    .players
    .read()
    .await
    .clone()
    .into_iter()
    .filter_map(|(u, p)| p.map(|p| (u, p)))
    .collect();
  players.sort_by(|a, b| a.1.cmp(&b.1, &sort_mode));
  Home {
    players,
    sort_mode,
    ver: env!("CARGO_PKG_VERSION"),
  }
}
