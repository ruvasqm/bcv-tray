#![windows_subsystem = "windows"] // Hide console window on Windows release builds

use image::{load_from_memory, Rgba, RgbaImage}; // Added load_from_memory
use imageproc::drawing::draw_text_mut;
use rusqlite::{params, Connection, Result as DbResult};
use rusttype::{Font, Scale};
use std::{
    env,
    fmt::Debug,
    // fs, // No longer needed for reading embedded assets directly
    // path::{Path, PathBuf}, // Path and PathBuf might still be needed for DB path
    path::PathBuf, // Keep for DB path
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
use rust_embed::RustEmbed;
use serde::Deserialize; // Import RustEmbed

// --- Asset Embedding ---
#[derive(RustEmbed)]
#[folder = "assets/"] // Path relative to Cargo.toml
struct Assets;

// --- Configuration ---
// Paths are now relative to the `assets/` directory defined in RustEmbed
const FONT_PATH: &str = "fonts/RobotoMonoNerdFont-Bold.ttf";
const ICON_HEIGHT: u32 = 16;
const PADDING: u32 = 4;
const UPDATE_INTERVAL_SECONDS: u64 = 1800;

const MONITOR_DOLAR_URL: &str = "https://api.monitordolarvenezuela.com/dolarhoy";
const CMC_BASE_URL: &str = "https://pro-api.coinmarketcap.com/v2/cryptocurrency/quotes/latest";
const CMC_BTC_ID: &str = "1";
const CMC_API_KEY_ENV_VAR: &str = "CMC_PRO_API_KEY";
const SATS_PER_BTC: f64 = 100_000_000.0;

// Icon paths are now relative to the `assets/` directory
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
    icon_asset_path: String, // Store the asset key (relative path for RustEmbed)
}

#[derive(Deserialize, Debug)]
struct MonitorDolarResponse {
    result: Vec<MonitorDolarEntry>,
}
#[derive(Deserialize, Debug)]
struct MonitorDolarEntry {
    bcv: String,
    binance: String,
}

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

// `find_asset_path` is no longer needed for embedded assets.
// If you had other non-embedded assets, you might keep a version of it.

fn main() {
    // --- Load Embedded Font ---
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
                "Mozilla/5.0 (X11; Ubuntu; Linux x86_64; rv:138.0) Gecko/20100101 Firefox/138.0",
            )
            .timeout(Duration::from_secs(15))
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
                name: Some("BCV/BIN/SAT Tray".to_string()),
                copyright: Some("ruvasqm".to_string()),
                ..Default::default()
            }),
        ),
        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    let mut tray_icon: Option<TrayIcon> = None;
    // Font is already loaded and Arc'd above

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

    let font_clone_main_loop = Arc::clone(&font); // Clone font for the main event loop
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
                        .with_icon(initial_icon).build().expect("Failed to build tray icon"),
                );
                println!("Placeholder tray icon created.");
                request_macos_redraw();
            }
            Event::UserEvent(UserEvent::UpdateTray) => {
                println!("Received UpdateTray event. Generating new icon...");
                if let Some(tray) = tray_icon.as_mut() {
                    let result = {
                        let db_guard = db_conn_mutex.lock().unwrap_or_else(|p| p.into_inner());
                        generate_tray_icon_image(&font_clone_main_loop, &db_guard) // Pass the font clone
                    };
                    match result {
                        Ok((new_icon, tooltip_text)) => {
                            if let Err(e) = tray.set_icon(Some(new_icon)) { eprintln!("Failed to set tray icon: {}", e); }
                            if let Err(e) = tray.set_tooltip(Some(tooltip_text)) { eprintln!("Failed to set tooltip: {}", e); }
                        }
                        Err(e) => {
                            eprintln!("Failed to generate updated icon: {}. Using fallback.", e);
                            let fallback_icon = create_fallback_icon(&font_clone_main_loop, "Error"); // Pass the font clone
                            if let Err(e) = tray.set_icon(Some(fallback_icon)) { eprintln!("Failed to set fallback tray icon: {}", e); }
                            if let Err(e) = tray.set_tooltip(Some("Error updating rates")) { eprintln!("Failed to set error tooltip: {}", e); }
                        }
                    }
                    request_macos_redraw();
                } else { println!("Tray icon not initialized yet, skipping update."); }
            }
            Event::UserEvent(UserEvent::MenuEvent(menu_event)) => {
                if menu_event.id == quit_i.id() {
                    tray_icon.take(); *control_flow = ControlFlow::Exit;
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
            Event::UserEvent(UserEvent::TrayIconEvent(_)) => { /*println!("Tray Event: {:?}", tray_event);*/ }
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

    match http_client
        .get(MONITOR_DOLAR_URL)
        .header("Accept", "*/*")
        .header("Referer", "https://monitordolarvenezuela.com/")
        .header("Origin", "https://monitordolarvenezuela.com")
        .send()
    {
        Ok(response) => {
            if response.status().is_success() {
                match response.json::<MonitorDolarResponse>() {
                    Ok(data) => {
                        if let Some(entry) = data.result.first() {
                            let conn_guard = db_conn_mutex
                                .lock()
                                .map_err(|e| format!("DB Mutex: {}", e))?;
                            let now_ts = Utc::now().to_rfc3339();
                            if let Ok(bcv) = entry.bcv.parse::<f64>() {
                                if conn_guard
                                    .execute(
                                        "INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)",
                                        params!["bcv", bcv, now_ts],
                                    )
                                    .is_ok()
                                {
                                    println!("Updated BCV: {}", bcv);
                                    an_update_succeeded = true;
                                } else {
                                    eprintln!("Failed to update BCV in DB");
                                }
                            }
                            if let Ok(bina) = entry.binance.parse::<f64>() {
                                if conn_guard
                                    .execute(
                                        "INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)",
                                        params!["binance", bina, now_ts],
                                    )
                                    .is_ok()
                                {
                                    println!("Updated Binance: {}", bina);
                                    an_update_succeeded = true;
                                } else {
                                    eprintln!("Failed to update Binance in DB");
                                }
                            }
                        } else {
                            eprintln!("MonitorDolar: No result entry.");
                        }
                    }
                    Err(e) => eprintln!("MonitorDolar JSON parse error: {}", e),
                }
            } else {
                eprintln!(
                    "MonitorDolar API fail: {}. Body: {:?}",
                    response.status(),
                    response.text().unwrap_or_default()
                );
            }
        }
        Err(e) => eprintln!("MonitorDolar fetch error: {}", e),
    }

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
                                .map_err(|e| format!("DB Mutex: {}", e))?;
                            let now_ts = Utc::now().to_rfc3339();
                            if conn_guard
                                .execute(
                                    "INSERT OR REPLACE INTO quotes VALUES(?1,?2,?3)",
                                    params!["satoshi", usd_price_satoshi, now_ts],
                                )
                                .is_ok()
                            {
                                println!(
                                    "Updated Satoshi (1 SAT in USD): {:.8}",
                                    usd_price_satoshi
                                );
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
        Err("No rates updated.".to_string())
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
                    icon_asset_path: icon_asset_key.to_string(), // Store the asset key
                });
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                println!("No rate for {} in DB.", symbol);
                rates_data.push(RateInfo {
                    currency: name.to_string(),
                    rate: 0.0, // Default if not found
                    icon_asset_path: icon_asset_key.to_string(),
                });
            }
            Err(e) => {
                eprintln!("DB fetch error for {}: {}", symbol, e);
                rates_data.push(RateInfo {
                    // Push a default on error to avoid crashing UI
                    currency: name.to_string(),
                    rate: 0.0,
                    icon_asset_path: icon_asset_key.to_string(),
                });
                // Optionally: return Err(e) if you want the whole icon generation to fail
            }
        }
    }
    Ok(rates_data)
}

/// Loads an icon from embedded assets, resizes it, and returns RGBA data.
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
        // Use the new function for embedded assets
        loaded_icons
            .push(load_and_resize_icon_from_embed(&rate_info.icon_asset_path, ICON_HEIGHT).ok());
    }

    #[cfg(target_os = "macos")]
    let tc = Rgba([255u8, 255u8, 255u8, 255u8]);
    #[cfg(not(target_os = "macos"))]
    let tc = Rgba([255u8, 255u8, 255u8, 255u8]);
    let scale = Scale::uniform(ICON_HEIGHT as f32 * 1.2);
    let vm = font.v_metrics(scale);
    let ty = ((ICON_HEIGHT as f32 - (vm.ascent - vm.descent)) / 2.0 + vm.ascent).round() as i32;

    let mut total_w = 0u32;
    let mut elements = Vec::new();
    let mut tooltips = Vec::new();

    for (i, rate_info) in rates.iter().enumerate() {
        let icon_img_opt = loaded_icons.get(i).and_then(|o| o.as_ref());
        let icon_w = icon_img_opt.map_or(ICON_HEIGHT / 2, |img| img.width().max(1));

        let text_str = format!("{:.2} ", rate_info.rate);
        tooltips.push(format!("{}: {}", rate_info.currency, text_str));

        let glyphs: Vec<_> = font
            .layout(&text_str, scale, rusttype::point(0.0, 0.0))
            .collect();
        let text_w = glyphs
            .iter()
            .rev()
            .filter_map(|g| g.pixel_bounding_box().map(|bb| bb.max.x))
            .max()
            .unwrap_or(0) as u32;
        let text_w_eff = text_w.max(10);

        let mut text_img = RgbaImage::from_pixel(text_w_eff, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
        draw_text_mut(
            &mut text_img,
            tc,
            0,
            ty - vm.ascent.abs().round() as i32,
            scale,
            font,
            &text_str,
        );

        if i > 0 {
            total_w = total_w.saturating_add(PADDING);
        }
        total_w = total_w.saturating_add(icon_w);
        total_w = total_w.saturating_add(PADDING);
        total_w = total_w.saturating_add(text_w_eff);
        elements.push((icon_img_opt.cloned(), Some(text_img)));
    }

    if total_w == 0 {
        // If all rates are 0.0 and no icons load, this could happen.
        // Fallback to a simple "Error" or "..." icon.
        println!("Calculated canvas width is zero, using fallback.");
        let fallback_icon = create_fallback_icon(font, "...");
        return Ok((fallback_icon, "Error generating icon".to_string()));
    }
    let mut canvas = RgbaImage::from_pixel(total_w, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
    let mut current_x: i64 = 0;

    for (i, (icon_opt, text_opt)) in elements.iter().enumerate() {
        if i > 0 {
            current_x += PADDING as i64;
        }
        if let Some(icon) = icon_opt {
            image::imageops::overlay(&mut canvas, icon, current_x, 0);
            current_x += icon.width() as i64;
        } else {
            current_x += (ICON_HEIGHT / 2) as i64;
        } // Advance even if icon is missing
        current_x += PADDING as i64;
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
    let scale = Scale::uniform(h as f32 * 0.7);
    #[cfg(target_os = "macos")]
    let tc = Rgba([255u8, 255, 255, 255]);
    #[cfg(not(target_os = "macos"))]
    let tc = Rgba([255u8, 255, 255, 255]);
    let bg = Rgba([0u8, 0, 0, 0]);
    let glyphs: Vec<_> = font
        .layout(text, scale, rusttype::point(0.0, 0.0))
        .collect();
    let tw = glyphs
        .last()
        .map(|g| g.position().x + g.unpositioned().h_metrics().advance_width)
        .unwrap_or(30.0);
    let w = (tw.ceil() as u32).max(10) + PADDING * 2;
    let mut canvas = RgbaImage::from_pixel(w, h, bg);
    let vm = font.v_metrics(scale);
    let ty = ((h as f32 - (vm.ascent - vm.descent)) / 2.0 + vm.ascent).round() as i32;
    draw_text_mut(
        &mut canvas,
        tc,
        PADDING as i32,
        ty - vm.descent.abs().round() as i32,
        scale,
        font,
        text,
    );
    TrayIconImage::from_rgba(canvas.into_raw(), w, h).expect("Fallback icon create failed")
}

#[cfg(target_os = "macos")]
fn request_macos_redraw() {
    use objc2_core_foundation::{CFRunLoopGetMain, CFRunLoopWakeUp};
    unsafe {
        if let Some(rl) = CFRunLoopGetMain() {
            CFRunLoopWakeUp(&rl);
        }
    }
}
#[cfg(not(target_os = "macos"))]
fn request_macos_redraw() {}
