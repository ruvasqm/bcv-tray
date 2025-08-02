#![windows_subsystem = "windows"] // Hide console window on Windows release builds

use image::{load_from_memory, Rgba, RgbaImage};
use imageproc::drawing::draw_text_mut;
use rusqlite::{params, Connection, Result as DbResult};
use rusttype::{Font, Scale};
use std::{
    env,
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon as TrayIconImage, TrayIcon, TrayIconBuilder, TrayIconEvent,
};

use chrono::Utc;
use reqwest::blocking::Client;
// --- MODIFIED ---: Added imports for Serialize and specific headers
use reqwest::header::{
    HeaderMap,
    HeaderValue,
    ACCEPT,
    ACCEPT_ENCODING,
    ACCEPT_LANGUAGE,
    CACHE_CONTROL,
    CONNECTION,
    CONTENT_TYPE,
    HOST,
    ORIGIN,
    PRAGMA,
    TE,
    USER_AGENT, // USER_AGENT added for specificity
};
use rust_embed::RustEmbed;
use scraper::{Html, Selector}; // For BCV
use serde::{Deserialize, Serialize}; // --- MODIFIED ---: Added Serialize

// --- Asset Embedding ---
#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

// --- Configuration ---
const FONT_PATH: &str = "fonts/RobotoMonoNerdFont-Bold.ttf";
const ICON_HEIGHT: u32 = 16;
const PADDING: u32 = 4;
const UPDATE_INTERVAL_SECONDS: u64 = 1800;

const BCV_URL: &str = "https://www.bcv.org.ve/";
const BCV_CSS_SELECTOR: &str = "html > body > div:nth-of-type(4) > div:nth-of-type(1) > div:nth-of-type(2) > div:nth-of-type(1) > div:nth-of-type(1) > div:nth-of-type(1) > section:nth-of-type(1) > div:nth-of-type(1) > div:nth-of-type(2) > div:nth-of-type(1) > div:nth-of-type(7) > div:nth-of-type(1) > div:nth-of-type(1) > div:nth-of-type(2) > strong";

const BINANCE_P2P_URL: &str = "https://p2p.binance.com/bapi/c2c/v2/friendly/c2c/adv/search";

const CMC_BASE_URL: &str = "https://pro-api.coinmarketcap.com/v2/cryptocurrency/quotes/latest";
const CMC_BTC_ID: &str = "1";
const CMC_API_KEY_ENV_VAR: &str = "CMC_PRO_API_KEY";
const SATS_PER_BTC: f64 = 100_000_000.0;

const CURRENCY_MAPPINGS: [(&str, &str, &str); 3] = [
    ("BCV", "ved.png", "bcv"),
    ("BIN", "binance.png", "binance"),
    ("SAT", "satoshi.png", "satoshi"),
];

// --- Data Structures ---
#[derive(Debug, Clone)]
struct RateInfo {
    currency: String,
    rate: f64,
    icon_asset_path: String,
}

#[derive(Serialize, Debug)]
#[serde(rename_all = "camelCase")]
struct BinanceP2PRequestPayload {
    asset: String,
    fiat: String,
    merchant_check: bool,
    page: u32,
    pay_types: Vec<String>,
    publisher_type: Option<String>, // Will be serialized as null if None
    rows: u32,
    trade_type: String,
}

#[derive(Deserialize, Debug)]
struct BinanceResponse {
    code: String,
    // message: Option<String>, // Not strictly needed for price extraction
    // messageDetail: Option<String>, // Not strictly needed
    data: Option<Vec<BinanceAdvContainer>>,
    success: bool,
}

#[derive(Deserialize, Debug)]
struct BinanceAdvContainer {
    adv: BinanceAdv,
}

#[derive(Deserialize, Debug)]
struct BinanceAdv {
    price: String, // Price is a string in the JSON
                   // ... other fields like advNo, tradeType etc. can be added if needed
}

// CMC Data Structures (unchanged)
#[derive(Deserialize, Debug)]
struct CmcResponse {
    data: CmcData,
}
#[derive(Deserialize, Debug)]
struct CmcData {
    #[serde(rename = "1")]
    btc: BtcQuoteContainer,
}
#[derive(Deserialize, Debug)]
struct BtcQuoteContainer {
    quote: UsdQuote,
}
#[derive(Deserialize, Debug)]
struct UsdQuote {
    #[serde(rename = "USD")]
    usd: PriceInfo,
}
#[derive(Deserialize, Debug)]
struct PriceInfo {
    price: f64,
}

#[allow(dead_code)]
enum UserEvent {
    TrayIconEvent(tray_icon::TrayIconEvent),
    MenuEvent(tray_icon::menu::MenuEvent),
    UpdateTray,
}

fn get_database_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .ok_or_else(|| "Could not find home directory".to_string())
        .map(|mut path| {
            path.push(".local/share/money/bin.db");
            path
        })
}

fn main() {
    let font_file = Assets::get(FONT_PATH)
        .unwrap_or_else(|| panic!("Critical Error: Embedded font not found: {}", FONT_PATH));
    let font_data = font_file.data.into_owned();
    let font = Arc::new(Font::try_from_vec(font_data).expect("Failed to parse embedded font"));
    println!("Embedded font '{}' loaded successfully.", FONT_PATH);

    let db_path = get_database_path().unwrap_or_else(|e| {
        eprintln!("Critical Error getting database path: {}", e);
        std::process::exit(1);
    });
    let db_path_str = db_path.to_str().unwrap_or_default().to_string();

    let http_client = Arc::new(
        Client::builder()
            .user_agent(
                // This is the default user agent for the client
                "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:138.0) Gecko/20100101 Firefox/138.0",
            )
            .timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(true) // Note: For BCV, might be needed. For Binance, likely not.
            .build()
            .expect("Failed to build HTTP client"),
    );

    let cmc_api_key = Arc::new(env::var(CMC_API_KEY_ENV_VAR).unwrap_or_else(|_| {
        eprintln!(
            "Warning: Env var {} not set. Satoshi updates will be skipped.",
            CMC_API_KEY_ENV_VAR
        );
        String::new()
    }));

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let proxy_clone_tray = proxy.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        proxy_clone_tray
            .send_event(UserEvent::TrayIconEvent(event))
            .ok();
    }));

    let proxy_clone_menu = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        proxy_clone_menu
            .send_event(UserEvent::MenuEvent(event))
            .ok();
    }));

    let tray_menu = Menu::new();
    let quit_i = MenuItem::new("Quit", true, None);
    let update_now_i = MenuItem::new("Update Now", true, None);
    let _ = tray_menu.append_items(&[
        &update_now_i,
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::about(
            None,
            Some(AboutMetadata {
                // --- MODIFIED ---: Updated name to include Binance
                name: Some("BCV/DOR/SAT/BIN Tray".to_string()),
                copyright: Some("ruvasqm".to_string()),
                ..Default::default()
            }),
        ),
        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    let mut tray_icon: Option<TrayIcon> = None;

    let db_conn = Connection::open(&db_path_str).expect("Failed to open database");
    initialize_database(&db_conn).expect("Failed to initialize database table");
    let db_conn_mutex = Arc::new(Mutex::new(db_conn));

    let proxy_clone_update = proxy.clone();
    let db_conn_mutex_bg = Arc::clone(&db_conn_mutex);
    let http_client_bg = Arc::clone(&http_client);
    let cmc_api_key_bg = Arc::clone(&cmc_api_key);
    thread::spawn(move || loop {
        println!("Background Task: Triggering data update...");
        match perform_data_update(&db_conn_mutex_bg, &http_client_bg, &cmc_api_key_bg) {
            Ok(_) => println!("Background Task: Data update process completed."),
            Err(e) => eprintln!("Background Task: Data update process failed: {}", e),
        }
        proxy_clone_update.send_event(UserEvent::UpdateTray).ok();
        thread::sleep(Duration::from_secs(UPDATE_INTERVAL_SECONDS));
    });

    let proxy_clone_init = proxy.clone();
    let db_conn_mutex_init = Arc::clone(&db_conn_mutex);
    let http_client_init = Arc::clone(&http_client);
    let cmc_api_key_init = Arc::clone(&cmc_api_key);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        println!("Initial Trigger: Triggering data update...");
        match perform_data_update(&db_conn_mutex_init, &http_client_init, &cmc_api_key_init) {
            Ok(_) => println!("Initial Trigger: Data update process completed."),
            Err(e) => eprintln!("Initial Trigger: Data update process failed: {}", e),
        }
        proxy_clone_init.send_event(UserEvent::UpdateTray).ok();
    });

    let font_clone_main_loop = Arc::clone(&font);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(tao::event::StartCause::Init) => {
                println!("App started, creating initial placeholder tray icon...");
                let initial_icon = create_fallback_icon(&font_clone_main_loop, "...");
                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_menu(Box::new(tray_menu.clone()))
                        .with_tooltip("Exchange Rates - Loading...")
                        .with_icon(initial_icon)
                        .build()
                        .expect("Failed to build tray icon"),
                );
                println!("Placeholder tray icon created.");
                request_macos_redraw();
            }
            Event::UserEvent(UserEvent::UpdateTray) => {
                println!("Received UpdateTray event. Generating new icon...");
                if let Some(tray) = tray_icon.as_mut() {
                    let result = {
                        let db_guard = db_conn_mutex.lock().unwrap_or_else(|p| p.into_inner());
                        generate_tray_icon_image(&font_clone_main_loop, &db_guard)
                    };
                    match result {
                        Ok((new_icon, tooltip_text)) => {
                            if let Err(e) = tray.set_icon(Some(new_icon)) {
                                eprintln!("Failed to set tray icon: {}", e);
                            }
                            if let Err(e) = tray.set_tooltip(Some(tooltip_text)) {
                                eprintln!("Failed to set tooltip: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to generate updated icon: {}. Using fallback.", e);
                            let fallback_icon =
                                create_fallback_icon(&font_clone_main_loop, "Error");
                            if let Err(e) = tray.set_icon(Some(fallback_icon)) {
                                eprintln!("Failed to set fallback tray icon: {}", e);
                            }
                            if let Err(e) = tray.set_tooltip(Some("Error updating rates")) {
                                eprintln!("Failed to set error tooltip: {}", e);
                            }
                        }
                    }
                    request_macos_redraw();
                } else {
                    println!("Tray icon not initialized yet, skipping update.");
                }
            }
            Event::UserEvent(UserEvent::MenuEvent(menu_event)) => {
                if menu_event.id == quit_i.id() {
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                } else if menu_event.id == update_now_i.id() {
                    let proxy_manual = proxy.clone();
                    let db_manual = Arc::clone(&db_conn_mutex);
                    let http_manual = Arc::clone(&http_client);
                    let key_manual = Arc::clone(&cmc_api_key);
                    thread::spawn(move || {
                        match perform_data_update(&db_manual, &http_manual, &key_manual) {
                            Ok(_) => println!("Manual Update: Data update process completed."),
                            Err(e) => eprintln!("Manual Update: Data update process failed: {}", e),
                        }
                        proxy_manual.send_event(UserEvent::UpdateTray).ok();
                    });
                }
            }
            Event::UserEvent(UserEvent::TrayIconEvent(_)) => {}
            _ => {}
        }
    });
}

fn initialize_database(conn: &Connection) -> DbResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS quotes (
            symbol TEXT PRIMARY KEY, rate REAL NOT NULL, last_updated TEXT NOT NULL
        )",
        [],
    )?;
    Ok(())
}

fn perform_data_update(
    db_conn_mutex: &Arc<Mutex<Connection>>,
    http_client: &Client,
    cmc_api_key: &str,
) -> Result<(), String> {
    println!("Performing data update from APIs...");
    let mut an_update_succeeded = false;

    // --- Fetch BCV rate from bcv.org.ve ---
    println!("Fetching BCV rate from {}", BCV_URL);
    match http_client.get(BCV_URL).send() {
        Ok(response) => {
            if response.status().is_success() {
                match response.text() {
                    Ok(html_content) => {
                        let document = Html::parse_document(&html_content);
                        match Selector::parse(BCV_CSS_SELECTOR) {
                            Ok(selector) => {
                                if let Some(element) = document.select(&selector).next() {
                                    let rate_str_raw =
                                        element.text().collect::<String>().trim().to_string();
                                    println!("BCV CSS selector raw string: '{}'", rate_str_raw);
                                    let rate_str_cleaned =
                                        rate_str_raw.replace(".", "").replace(",", ".");
                                    match rate_str_cleaned.parse::<f64>() {
                                        Ok(bcv_rate) => {
                                            let conn_guard = db_conn_mutex
                                                .lock()
                                                .map_err(|e| format!("DB Mutex for BCV: {}", e))?;
                                            let now_ts = Utc::now().to_rfc3339();
                                            if conn_guard.execute("INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)", params!["bcv", bcv_rate, now_ts]).is_ok() {
                                                println!("Updated BCV from bcv.org.ve: {}", bcv_rate);
                                                an_update_succeeded = true;
                                            } else { eprintln!("Failed to update BCV in DB (from bcv.org.ve)"); }
                                        }
                                        Err(e) => eprintln!(
                                            "BCV: Failed to parse rate string '{}' to f64: {}",
                                            rate_str_cleaned, e
                                        ),
                                    }
                                } else {
                                    eprintln!(
                                        "BCV: CSS selector '{}' did not find any node.",
                                        BCV_CSS_SELECTOR
                                    );
                                }
                            }
                            Err(e) => {
                                eprintln!(
                                    "BCV: Failed to parse CSS selector '{}': {:?}",
                                    BCV_CSS_SELECTOR, e
                                );
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("BCV: Failed to read response text from {}: {}", BCV_URL, e);
                    }
                }
            } else {
                eprintln!(
                    "BCV API request to {} failed with status: {}. Body: {:?}",
                    BCV_URL,
                    response.status(),
                    response
                        .text()
                        .unwrap_or_else(|_| "Failed to read error body".to_string())
                );
            }
        }
        Err(e) => {
            eprintln!("BCV fetch error for {}: {}", BCV_URL, e);
        }
    }

    // --- Fetch Binance P2P rate ---
    println!("Fetching Binance P2P rate from {}", BINANCE_P2P_URL);
    let binance_payload = BinanceP2PRequestPayload {
        asset: "USDT".to_string(),
        fiat: "VES".to_string(),
        merchant_check: false, // Corresponds to Python `False`
        page: 1,
        pay_types: vec!["PagoMovil".to_string()],
        publisher_type: None, // Corresponds to Python `None`, will be JSON `null`
        rows: 1,
        trade_type: "SELL".to_string(),
    };

    let mut binance_headers = HeaderMap::new();
    binance_headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    binance_headers.insert(
        ACCEPT_ENCODING,
        HeaderValue::from_static("gzip, deflate, br"),
    ); // reqwest handles decompression
    binance_headers.insert(
        ACCEPT_LANGUAGE,
        HeaderValue::from_static("en-GB,en-US;q=0.9,en;q=0.8"),
    );
    binance_headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    binance_headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
    binance_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json")); // Crucial for .json() payload
    binance_headers.insert(HOST, HeaderValue::from_static("p2p.binance.com"));
    binance_headers.insert(ORIGIN, HeaderValue::from_static("https://p2p.binance.com"));
    binance_headers.insert(PRAGMA, HeaderValue::from_static("no-cache"));
    binance_headers.insert(TE, HeaderValue::from_static("Trailers"));
    binance_headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:88.0) Gecko/20100101 Firefox/88.0",
        ),
    ); // Specific User-Agent from curl

    match http_client
        .post(BINANCE_P2P_URL)
        .headers(binance_headers)
        .json(&binance_payload)
        .send()
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<BinanceResponse>() {
                    Ok(binance_api_response) => {
                        if binance_api_response.success && binance_api_response.code == "000000" {
                            if let Some(ref data_vec) = binance_api_response.data {
                                if let Some(first_adv_container) = data_vec.get(0) {
                                    match first_adv_container.adv.price.parse::<f64>() {
                                        Ok(binance_rate) => {
                                            let conn_guard = db_conn_mutex.lock().map_err(|e| {
                                                format!("DB Mutex for Binance P2P: {}", e)
                                            })?;
                                            let now_ts = Utc::now().to_rfc3339();
                                            if conn_guard
                                                .execute(
                                                    "INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)",
                                                    params!["binance", binance_rate, now_ts],
                                                )
                                                .is_ok()
                                            {
                                                println!("Updated Binance P2P (USDT/VES): {}", binance_rate);
                                                an_update_succeeded = true;
                                            } else {
                                                eprintln!("Failed to update Binance P2P in DB");
                                            }
                                        }
                                        Err(e) => eprintln!(
                                            "Binance P2P: Failed to parse price string '{}' to f64: {}",
                                            first_adv_container.adv.price, e
                                        ),
                                    }
                                } else {
                                    eprintln!("Binance P2P: 'data' array is empty in API response. Full response: {:?}", binance_api_response);
                                }
                            } else {
                                eprintln!("Binance P2P: 'data' field is null or missing in API response. Full response: {:?}", binance_api_response);
                            }
                        } else {
                            eprintln!("Binance P2P API call reported not successful or wrong code. Code: {}, Success: {}. Full response: {:?}", binance_api_response.code, binance_api_response.success, binance_api_response);
                        }
                    }
                    Err(e) => {
                        eprintln!("Binance P2P API JSON parse error: {}", e);
                    }
                }
            } else {
                eprintln!(
                    "Binance P2P API request failed with status: {}. Body: {:?}",
                    response.status(),
                    response
                        .text()
                        .unwrap_or_else(|_| "Failed to read error body".to_string())
                );
            }
        }
        Err(e) => {
            eprintln!("Binance P2P API fetch error: {}", e);
        }
    }

    // --- CMC Satoshi Fetching Logic (remains unchanged) ---
    if !cmc_api_key.is_empty() {
        let cmc_url = format!("{}?id={}", CMC_BASE_URL, CMC_BTC_ID);
        match http_client
            .get(&cmc_url)
            .header("X-CMC_PRO_API_KEY", cmc_api_key)
            .header("Accept", "application/json")
            .send()
        {
            Ok(response) => {
                if response.status().is_success() {
                    match response.json::<CmcResponse>() {
                        Ok(data) => {
                            let btc_price_usd = data.data.btc.quote.usd.price;
                            let usd_price_satoshi = SATS_PER_BTC / btc_price_usd;
                            let conn_guard = db_conn_mutex
                                .lock()
                                .map_err(|e| format!("DB Mutex for CMC: {}", e))?;
                            let now_ts = Utc::now().to_rfc3339();
                            if conn_guard
                                .execute(
                                    "INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)",
                                    params!["satoshi", usd_price_satoshi, now_ts],
                                )
                                .is_ok()
                            {
                                println!("Updated Satoshi (SAT per USD): {:.2}", usd_price_satoshi);
                                an_update_succeeded = true;
                            } else {
                                eprintln!("Failed to update Satoshi in DB");
                            }
                        }
                        Err(e) => eprintln!("CMC JSON parse error: {}", e),
                    }
                } else {
                    eprintln!(
                        "CMC API fail: {}. Body: {:?}",
                        response.status(),
                        response.text().unwrap_or_default()
                    );
                }
            }
            Err(e) => eprintln!("CMC fetch error: {}", e),
        }
    }

    if an_update_succeeded {
        Ok(())
    } else {
        Err("No rates were successfully updated.".to_string())
    }
}

fn fetch_rates(conn: &Connection) -> DbResult<Vec<RateInfo>> {
    let mut rates_data = Vec::new();
    for (name, icon_asset_key, symbol) in CURRENCY_MAPPINGS.iter() {
        match conn.query_row(
            "SELECT rate FROM quotes WHERE symbol=?1 ORDER BY last_updated DESC LIMIT 1",
            params![symbol],
            |row| row.get(0),
        ) {
            Ok(rate_value) => {
                rates_data.push(RateInfo {
                    currency: name.to_string(),
                    rate: rate_value,
                    icon_asset_path: icon_asset_key.to_string(),
                });
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                println!("No rate for {} in DB.", symbol);
                rates_data.push(RateInfo {
                    currency: name.to_string(),
                    rate: 0.0, // Default to 0.0 if no data
                    icon_asset_path: icon_asset_key.to_string(),
                });
            }
            Err(e) => {
                eprintln!("DB fetch error for {}: {}", symbol, e);
                rates_data.push(RateInfo {
                    currency: name.to_string(),
                    rate: 0.0, // Default to 0.0 on error
                    icon_asset_path: icon_asset_key.to_string(),
                });
            }
        }
    }
    Ok(rates_data)
}

fn load_and_resize_icon_from_embed(
    asset_key: &str,
    target_height: u32,
) -> Result<RgbaImage, String> {
    if target_height == 0 {
        return Err("Target height 0".to_string());
    }
    let asset_file = Assets::get(asset_key)
        .ok_or_else(|| format!("Embedded icon not found: '{}'", asset_key))?;
    let img_data = asset_file.data;
    let img = load_from_memory(&img_data)
        .map_err(|e| format!("Failed to decode embedded icon '{}': {}", asset_key, e))?
        .into_rgba8();
    let (w, h) = img.dimensions();
    if h == 0 || w == 0 {
        return Err(format!("Embedded icon zero dim: '{}'", asset_key));
    }
    let aspect = w as f32 / h as f32;
    let new_w = (target_height as f32 * aspect).round() as u32;
    Ok(image::imageops::resize(
        &img,
        new_w.max(1),
        target_height,
        image::imageops::FilterType::Lanczos3,
    ))
}

fn generate_tray_icon_image(
    font: &Arc<Font>,
    db_conn: &Connection,
) -> Result<(TrayIconImage, String), Box<dyn std::error::Error>> {
    let rates = fetch_rates(db_conn)?;
    if rates.is_empty() {
        let fallback = create_fallback_icon(font, "No Data");
        return Ok((fallback, "No data".to_string()));
    }

    let mut loaded_icons = Vec::new();
    for rate_info in &rates {
        loaded_icons
            .push(load_and_resize_icon_from_embed(&rate_info.icon_asset_path, ICON_HEIGHT).ok());
    }

    #[cfg(target_os = "macos")]
    let tc = Rgba([255u8, 255u8, 255u8, 255u8]);

    #[cfg(not(target_os = "macos"))]
    let tc = Rgba([255u8, 255u8, 255u8, 255u8]); // Text color

    let scale = Scale::uniform(ICON_HEIGHT as f32 * 1.2); // Slightly larger for better fit
    let vm = font.v_metrics(scale);
    let ty = ((ICON_HEIGHT as f32 - (vm.ascent - vm.descent)) / 2.0 + vm.ascent).round() as i32;

    let mut total_w = 0u32;
    let mut elements = Vec::new();
    let mut tooltips = Vec::new();

    for (i, rate_info) in rates.iter().enumerate() {
        let icon_img_opt = loaded_icons.get(i).and_then(|o| o.as_ref());
        let icon_w = icon_img_opt.map_or(ICON_HEIGHT / 2, |img| img.width().max(1)); // Placeholder width if icon fails
        let text_str = format!("{:.2}  ", rate_info.rate); // Add padding to text
        tooltips.push(format!("{}: {}", rate_info.currency, text_str.trim()));
        let glyphs: Vec<_> = font
            .layout(&text_str, scale, rusttype::point(0.0, 0.0))
            .collect();
        let text_w = glyphs
            .iter()
            .rev()
            .filter_map(|g| g.pixel_bounding_box().map(|bb| bb.max.x))
            .max()
            .unwrap_or(0) as u32;
        let text_w_eff = text_w.max(10); // Min text width
        let mut text_img = RgbaImage::from_pixel(text_w_eff, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
        draw_text_mut(
            &mut text_img,
            tc,
            0,                                   // x position for text within its own image
            ty - vm.ascent.abs().round() as i32, // y position for text (adjust based on font metrics)
            scale,
            font,
            &text_str,
        );
        if i > 0 {
            total_w = total_w.saturating_add(PADDING);
        }
        total_w = total_w.saturating_add(icon_w);
        total_w = total_w.saturating_add(PADDING); // Padding between icon and text
        total_w = total_w.saturating_add(text_w_eff);
        elements.push((icon_img_opt.cloned(), Some(text_img)));
    }

    if total_w == 0 {
        println!("Calculated canvas width is zero, using fallback.");
        let fallback_icon = create_fallback_icon(font, "...");
        return Ok((fallback_icon, "Error generating icon".to_string()));
    }
    total_w = total_w.max(1); // Ensure width is at least 1
    let mut canvas = RgbaImage::from_pixel(total_w, ICON_HEIGHT, Rgba([0, 0, 0, 0])); // Transparent background
    let mut current_x: i64 = 0;
    for (i, (icon_opt, text_opt)) in elements.iter().enumerate() {
        if i > 0 {
            current_x += PADDING as i64; // Padding between currency groups
        }
        if let Some(icon) = icon_opt {
            image::imageops::overlay(&mut canvas, icon, current_x, 0);
            current_x += icon.width() as i64;
        } else {
            // If icon failed to load, still advance X to keep spacing somewhat consistent
            current_x += (ICON_HEIGHT / 2) as i64;
        }
        current_x += PADDING as i64; // Padding between icon and text
        if let Some(text) = text_opt {
            image::imageops::overlay(&mut canvas, text, current_x, 0);
            current_x += text.width() as i64;
        }
    }
    Ok((
        TrayIconImage::from_rgba(canvas.into_raw(), total_w, ICON_HEIGHT)?,
        tooltips.join(" | "),
    ))
}

fn create_fallback_icon(font: &Arc<Font>, text: &str) -> TrayIconImage {
    let h = ICON_HEIGHT;
    let scale = Scale::uniform(h as f32 * 0.7); // Smaller text for fallback

    #[cfg(target_os = "macos")]
    let tc = Rgba([255u8, 255, 255, 255]); // White text

    #[cfg(not(target_os = "macos"))]
    let tc = Rgba([255u8, 255, 255, 255]); // White text
    let bg = Rgba([0u8, 0, 0, 0]); // Transparent background

    // Calculate text width
    let glyphs: Vec<_> = font
        .layout(text, scale, rusttype::point(0.0, 0.0))
        .collect();
    let tw = glyphs
        .last()
        .map(|g| g.position().x + g.unpositioned().h_metrics().advance_width)
        .unwrap_or(30.0); // Default width if no glyphs
    let w = (tw.ceil() as u32).max(10) + PADDING * 2; // Add padding

    let mut canvas = RgbaImage::from_pixel(w, h, bg);

    // Calculate text y position for vertical centering
    let vm = font.v_metrics(scale);
    let ty = ((h as f32 - (vm.ascent - vm.descent)) / 2.0 + vm.ascent).round() as i32;

    draw_text_mut(
        &mut canvas,
        tc,
        PADDING as i32,                       // X position with padding
        ty - vm.descent.abs().round() as i32, // Y position, adjust for font metrics
        scale,
        font,
        text,
    );
    TrayIconImage::from_rgba(canvas.into_raw(), w, h).expect("Fallback icon create failed")
}

#[cfg(target_os = "macos")]
fn request_macos_redraw() {
    extern "C" {
        fn CFRunLoopGetMain() -> *const std::ffi::c_void;
        fn CFRunLoopWakeUp(rl: *const std::ffi::c_void);
    }
    let rl = unsafe { CFRunLoopGetMain() };
    if !rl.is_null() {
        unsafe { CFRunLoopWakeUp(rl) };
    }
}
#[cfg(not(target_os = "macos"))]
fn request_macos_redraw() {}
