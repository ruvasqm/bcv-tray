#![windows_subsystem = "windows"] // Hide console window on Windows release builds

// Remove unused: ImageBuffer
use image::{Rgba, RgbaImage}; // Keep Rgba, RgbaImage
use imageproc::drawing::draw_text_mut;
use rusttype::{Font, Scale};
// Remove unused: Row
use rusqlite::{Connection, Result as DbResult}; // Keep Connection, DbResult
use std::{
    env,
    fmt::Debug,
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tao::{
    event::Event,
    // Remove unused: EventLoopProxy
    event_loop::{ControlFlow, EventLoopBuilder},
};
use tray_icon::{
    menu::{AboutMetadata, Menu, MenuEvent, MenuItem, PredefinedMenuItem},
    Icon as TrayIconImage, // Alias tray_icon::Icon
    TrayIcon,              // Import TrayIcon directly
    TrayIconBuilder,
    TrayIconEvent,
};

// --- Configuration ---
const FONT_PATH: &str = "/home/ruvasqm/dev/bcv-tray/fonts/RobotoMonoNerdFont-Bold.ttf"; // Make sure this font exists in a 'fonts' subdir or adjust path
const ICON_HEIGHT: u32 = 16; // Typical tray icon height
const PADDING: u32 = 4; // Padding between elements
const UPDATE_INTERVAL_SECONDS: u64 = 1800; // Update every 30 minutes
                                           // Use absolute path for Python script as provided by user
const PYTHON_SCRIPT_PATH: &str = "/home/ruvasqm/dev/binance/bin.py";

// Define the icons and their corresponding COLUMN NAME in the DB query result
// These names MUST match the SELECT clause in fetch_rates
const CURRENCY_MAPPINGS: [(&str, &str, &str); 3] = [
    ("BCV", "/home/ruvasqm/dev/bcv-tray/ved.png", "bcv"), // Display "BCV", use ved.png, get data from 'usd' column
    ("BIN", "/home/ruvasqm/dev/bcv-tray/binance.png", "binance"), // Display "BIN", use binance.png, get data from 'eur' column
    ("SAT", "/home/ruvasqm/dev/bcv-tray/satoshi.png", "satoshi"), // Display "SAT", use satoshi.png, get data from 'bcv' column
];

// --- Data Structure ---
#[derive(Debug, Clone)]
struct RateInfo {
    currency: String,
    rate: f64,
    icon_path: String, // Store the resolved, absolute path
}

// --- User Events ---
enum UserEvent {
    TrayIconEvent(tray_icon::TrayIconEvent),
    MenuEvent(tray_icon::menu::MenuEvent),
    UpdateTray, // Signal to refresh data and icon
}

// --- Helper Functions ---

/// Helper function to get the absolute path to the database in ~/.local/share/money/
fn get_database_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .ok_or_else(|| "Could not find home directory".to_string())
        .map(|mut path| {
            path.push(".local/share/money/bin.db");
            path
        })
}

/// Helper function to find icons/fonts relative to executable or manifest dir
/// (Will not be used for the absolute PYTHON_SCRIPT_PATH)
fn find_asset_path(asset_filename: &str) -> Result<PathBuf, String> {
    // 1. Try relative to executable's directory
    if let Ok(mut exe_path) = env::current_exe() {
        exe_path.pop(); // Go to directory containing executable
        let asset_in_exe_dir = exe_path.join(asset_filename);
        if asset_in_exe_dir.exists() {
            return Ok(asset_in_exe_dir);
        }
        // 2. Try one level up from exe dir (common for target/debug structure)
        if let Some(parent_dir) = exe_path.parent() {
            let asset_in_parent_dir = parent_dir.join(asset_filename);
            if asset_in_parent_dir.exists() {
                return Ok(asset_in_parent_dir);
            }
        }
    }
    // 3. Try relative to CARGO_MANIFEST_DIR (useful during `cargo run`)
    if let Ok(manifest_dir_str) = env::var("CARGO_MANIFEST_DIR") {
        let mut path_in_manifest = PathBuf::from(manifest_dir_str);
        path_in_manifest.push(asset_filename);
        if path_in_manifest.exists() {
            return Ok(path_in_manifest);
        }
    }
    // 4. Fallback: Assume it's in the current working directory
    let cwd_path = PathBuf::from(asset_filename);
    if cwd_path.exists() {
        return Ok(cwd_path);
    }
    Err(format!(
        "Asset '{}' not found relative to executable, parent dir, manifest dir, or cwd.",
        asset_filename
    ))
}

fn main() {
    // --- Initial Setup: Find Assets ---
    let font_path_buf = find_asset_path(FONT_PATH).unwrap_or_else(|e| {
        eprintln!("Critical Error: Font not found: {}", e);
        eprintln!(
            "Please ensure '{}' exists relative to the executable, project root, or working directory.",
            FONT_PATH
        );
        std::process::exit(1);
    });
    // Remove unused variable: font_path_str
    // let font_path_str = font_path_buf.to_str().unwrap_or_default();

    // Python script path is absolute, no need to find it relatively
    let python_script_path_str = PYTHON_SCRIPT_PATH.to_string();
    if !Path::new(&python_script_path_str).exists() {
        eprintln!(
            "Critical Error: Python script not found at absolute path: {}",
            python_script_path_str
        );
        std::process::exit(1);
    }

    // Get Database Path
    let db_path = match get_database_path() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("Critical Error getting database path: {}", e);
            std::process::exit(1);
        }
    };
    if !db_path.exists() {
        println!(
            "Warning: Database file not found at '{}'. The python script needs to create and populate it.",
            db_path.display()
        );
    }
    let db_path_str = db_path.to_str().unwrap_or_default().to_string();

    // --- Event Loop Setup ---
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // --- Tray Event Handlers (forwarding to event loop) ---
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

    // --- Menu Setup ---
    let tray_menu = Menu::new();
    let quit_i = MenuItem::new("Quit", true, None);
    let update_now_i = MenuItem::new("Update Now", true, None);
    tray_menu.append_items(&[
        &update_now_i,
        &PredefinedMenuItem::separator(),
        &PredefinedMenuItem::about(
            None,
            Some(AboutMetadata {
                name: Some("BCV/BIN/SAT Tray".to_string()), // Customize App Name
                copyright: Some("ruvasqm".to_string()),     // Your Name/Company
                ..Default::default()
            }),
        ),
        &PredefinedMenuItem::separator(),
        &quit_i,
    ]);

    // --- State Variables ---
    let mut tray_icon: Option<TrayIcon> = None;
    // Use font_path_buf directly here
    let font_data = fs::read(&font_path_buf).expect("Failed to read font file");
    let font = Arc::new(Font::try_from_vec(font_data).expect("Failed to parse font"));

    let db_conn_mutex = Arc::new(Mutex::new(
        Connection::open(&db_path_str).expect("Failed to open database"),
    ));

    // --- Background Update Thread ---
    let proxy_clone_update = proxy.clone();
    let python_script_path_clone = python_script_path_str.clone();
    thread::spawn(move || loop {
        println!(
            "Background Task: Running Python script '{}'...",
            &python_script_path_clone
        );
        match run_python_script(&python_script_path_clone) {
            Ok(_) => {
                println!("Background Task: Python script finished successfully.");
                proxy_clone_update.send_event(UserEvent::UpdateTray).ok();
            }
            Err(e) => {
                eprintln!("Background Task: Failed to run Python script: {}", e);
            }
        }
        thread::sleep(Duration::from_secs(UPDATE_INTERVAL_SECONDS));
    });

    // Trigger initial update after short delay
    let proxy_clone_init = proxy.clone();
    let python_script_path_init = python_script_path_str.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        println!("Initial Trigger: Running Python script...");
        match run_python_script(&python_script_path_init) {
            Ok(_) => {
                println!("Initial Trigger: Python script finished successfully.");
                proxy_clone_init.send_event(UserEvent::UpdateTray).ok();
            }
            Err(e) => {
                eprintln!("Initial Trigger: Failed to run Python script: {}", e);
                proxy_clone_init.send_event(UserEvent::UpdateTray).ok();
            }
        }
    });

    // --- Event Loop ---
    let font_clone = Arc::clone(&font);
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;

        match event {
            Event::NewEvents(tao::event::StartCause::Init) => {
                println!("App started, creating initial placeholder tray icon...");
                let initial_icon = create_fallback_icon(&font_clone, "...");

                tray_icon = Some(
                    TrayIconBuilder::new()
                        .with_menu(Box::new(tray_menu.clone()))
                        .with_tooltip("Exchange Rates")
                        .with_icon(initial_icon)
                        .build()
                        .expect("Failed to build tray icon"),
                );
                println!("Placeholder tray icon created.");

                #[cfg(target_os = "macos")]
                request_macos_redraw();
            }

            Event::UserEvent(UserEvent::UpdateTray) => {
                println!("Received UpdateTray event. Generating new icon...");
                if let Some(tray) = tray_icon.as_mut() {
                    let result = {
                        let db_guard = match db_conn_mutex.lock() {
                            Ok(guard) => guard,
                            Err(poisoned) => {
                                eprintln!("Database mutex poisoned! Recovering.");
                                poisoned.into_inner()
                            }
                        };
                        generate_tray_icon_image(&font_clone, &db_guard)
                    };

                    match result {
                        Ok((new_icon, tooltip_text)) => {
                            if let Err(e) = tray.set_icon(Some(new_icon)) {
                                eprintln!("Failed to set tray icon: {}", e);
                            } else {
                                //println!("Tray icon updated successfully."); // Less verbose logging
                            }
                            if let Err(e) = tray.set_tooltip(Some(tooltip_text)) {
                                eprintln!("Failed to set tooltip: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("Failed to generate updated icon: {}. Using fallback.", e);
                            let fallback_icon = create_fallback_icon(&font_clone, "Error");
                            if let Err(e_set) = tray.set_icon(Some(fallback_icon)) {
                                eprintln!("Failed to set fallback tray icon: {}", e_set);
                            }
                            if let Err(e_tip) = tray.set_tooltip(Some("Error updating rates")) {
                                eprintln!("Failed to set error tooltip: {}", e_tip);
                            }
                        }
                    }
                } else {
                    println!("Tray icon not initialized yet, skipping update.");
                }
            }

            Event::UserEvent(UserEvent::MenuEvent(event)) => {
                println!("Menu Event: {:?}", event.id);
                if event.id == quit_i.id() {
                    println!("Quit item selected.");
                    tray_icon.take();
                    *control_flow = ControlFlow::Exit;
                } else if event.id == update_now_i.id() {
                    println!("Update Now selected.");
                    let proxy_clone = proxy.clone();
                    let script_path_clone = python_script_path_str.clone();
                    thread::spawn(move || {
                        println!("Manual Update: Running Python script...");
                        match run_python_script(&script_path_clone) {
                            Ok(_) => {
                                println!("Manual Update: Python script finished.");
                                proxy_clone.send_event(UserEvent::UpdateTray).ok();
                            }
                            Err(e) => {
                                eprintln!("Manual Update: Failed to run Python script: {}", e);
                                proxy_clone.send_event(UserEvent::UpdateTray).ok();
                            }
                        }
                    });
                }
            }

            Event::UserEvent(UserEvent::TrayIconEvent(event)) => {
                println!("Tray Event: {:?}", event);
            }

            _ => {}
        }
    });
}

/// Runs the specified Python script. (Keep this function as is)
fn run_python_script(script_path: &str) -> std::io::Result<()> {
    // Try common python commands
    let commands_to_try = [
        "/home/ruvasqm/dev/binance/.venv/bin/python3",
        "python3",
        "python",
    ];
    let mut success = false;
    let mut last_error: Option<std::io::Error> = None;

    for command in commands_to_try {
        // println!( // Less verbose
        //     "Attempting to run script with: {} {}",
        //     command, script_path
        // );
        let mut cmd = Command::new(command);
        cmd.arg(script_path);
        // Let script handle its own working directory based on its absolute path
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        match cmd.spawn() {
            Ok(mut child) => {
                match child.wait() {
                    Ok(status) => {
                        if status.success() {
                            // println!("Script executed successfully with '{}'.", command); // Less verbose
                            // Optional: Read stdout/stderr even on success if needed
                            success = true;
                            break;
                        } else {
                            eprintln!("Script failed with status: {} using '{}'.", status, command);
                            if let Some(stderr) = child.stderr {
                                match std::io::read_to_string(stderr) {
                                    Ok(err_out) if !err_out.is_empty() => {
                                        eprintln!("Script stderr:\n{}", err_out)
                                    }
                                    Ok(_) => {} // No stderr output
                                    Err(e) => eprintln!("Failed to read script stderr: {}", e),
                                }
                            }
                            last_error = Some(std::io::Error::new(
                                std::io::ErrorKind::Other,
                                format!("Script exited with non-zero status: {}", status),
                            ));
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "Failed to wait for script executed with '{}': {}",
                            command, e
                        );
                        last_error = Some(e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // println!("Command '{}' not found, trying next.", command); // Less verbose
                last_error = Some(e);
            }
            Err(e) => {
                eprintln!("Failed to spawn script with '{}': {}", command, e);
                last_error = Some(e);
            }
        }
    }

    if success {
        Ok(())
    } else {
        Err(last_error.unwrap_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::Other,
                "Python script execution failed or Python interpreter not found.",
            )
        }))
    }
}

/// Loads data from the SQLite database. (Keep this function as is)
fn fetch_rates(conn: &Connection) -> DbResult<Vec<RateInfo>> {
    let query =
        "SELECT timestamp, bcv, binance, satoshi FROM money ORDER BY timestamp DESC LIMIT 1";
    // println!("Executing DB Query: {}", query); // Less verbose
    let mut stmt = conn.prepare(query)?;
    let mut rates = Vec::new();

    let result = stmt.query_row([], |row| {
        let mut fetched_rates = Vec::with_capacity(CURRENCY_MAPPINGS.len());
        let timestamp_str: String = row.get("timestamp").unwrap_or_else(|_| "N/A".to_string());

        for (name, icon_file, col_name) in CURRENCY_MAPPINGS.iter() {
            let rate_value: f64 = match row.get(*col_name) {
                Ok(val) => val,
                Err(e) => {
                    eprintln!(
                        "Error getting rate from column '{}': {}. Using 0.0",
                        col_name, e
                    );
                    0.0
                }
            };

            let icon_path_str = match find_asset_path(icon_file) {
                Ok(path_buf) => path_buf.to_string_lossy().to_string(),
                Err(e) => {
                    eprintln!("Icon Error: {}", e);
                    find_asset_path("missing.png")
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| icon_file.to_string())
                }
            };

            fetched_rates.push(RateInfo {
                currency: name.to_string(),
                rate: rate_value,
                icon_path: icon_path_str,
            });
        }
        Ok((fetched_rates, timestamp_str))
    });

    match result {
        Ok((fetched_rates, timestamp)) => {
            println!("Fetched rates ({}): {:?}", timestamp, fetched_rates);
            rates = fetched_rates;
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            println!("No rows found in the 'money' table.");
        }
        Err(e) => {
            eprintln!("Failed to fetch rates from database: {}", e);
            return Err(e);
        }
    }
    Ok(rates)
}

/// Loads an icon from the resolved path, resizes it, and returns RGBA data. (Keep this function as is)
fn load_and_resize_icon(path_str: &str, target_height: u32) -> Result<RgbaImage, String> {
    let img_path = Path::new(path_str);
    if !img_path.exists() {
        return Err(format!(
            "Icon file not found at resolved path: '{}'",
            path_str
        ));
    }
    if target_height == 0 {
        return Err("Target height cannot be zero".to_string());
    }

    let img = match image::open(img_path) {
        Ok(i) => i.into_rgba8(),
        Err(e) => {
            return Err(format!(
                "Failed to open or decode icon '{}': {}",
                path_str, e
            ))
        }
    };

    let (width, height) = img.dimensions();
    if height == 0 || width == 0 {
        return Err(format!("Icon has zero dimensions: '{}'", path_str));
    }

    let aspect_ratio = width as f32 / height as f32;
    let new_width = (target_height as f32 * aspect_ratio).round() as u32;
    let final_width = new_width.max(1);

    let resized_img = image::imageops::resize(
        &img,
        final_width,
        target_height,
        image::imageops::FilterType::Lanczos3,
    );
    Ok(resized_img)
}

/// Generates the composite tray icon image and a tooltip string.
fn generate_tray_icon_image(
    font: &Arc<Font>,
    db_conn: &Connection,
) -> Result<(TrayIconImage, String), Box<dyn std::error::Error>> {
    let rates = fetch_rates(db_conn)?;
    if rates.is_empty() {
        println!("No rates found in DB, creating fallback icon.");
        let fallback_icon = create_fallback_icon(font, "No Data");
        return Ok((fallback_icon, "No data available".to_string()));
    }

    let mut loaded_icons = Vec::new();
    for rate_info in &rates {
        match load_and_resize_icon(&rate_info.icon_path, ICON_HEIGHT) {
            Ok(icon_img) => loaded_icons.push(Some(icon_img)),
            Err(e) => {
                eprintln!("Error loading icon for {}: {}", rate_info.currency, e);
                loaded_icons.push(None);
            }
        }
    }

    #[cfg(target_os = "macos")]
    let text_color = Rgba([255u8, 255u8, 255u8, 255u8]);
    #[cfg(not(target_os = "macos"))]
    let text_color = Rgba([255u8, 255u8, 255u8, 255u8]);

    let scale = Scale::uniform(ICON_HEIGHT as f32 * 1.2);
    let v_metrics = font.v_metrics(scale);
    let text_y_offset = ((ICON_HEIGHT as f32 - (v_metrics.ascent - v_metrics.descent)) / 2.0
        + v_metrics.ascent)
        .round() as i32;

    let mut total_width = 0u32;
    let mut rendered_elements = Vec::new();
    let mut tooltip_parts = Vec::new();

    for (i, rate_info) in rates.iter().enumerate() {
        let icon_image_opt = loaded_icons[i].as_ref();
        let icon_width = icon_image_opt.map_or(ICON_HEIGHT / 2, |img| img.width().max(1));

        let text = format!("{:.2}", rate_info.rate);
        tooltip_parts.push(format!("{}: {:.2}", rate_info.currency, rate_info.rate));

        let glyphs: Vec<_> = font
            .layout(&text, scale, rusttype::point(0.0, 0.0))
            .collect();

        // *** FIX SYNTAX ERROR HERE ***
        let text_width = (glyphs
            .last()
            .map(|g| g.position().x + g.unpositioned().h_metrics().advance_width)
            .unwrap_or(0.0)
            .ceil() as u32) // Cast here
            .max(1); // Apply max to the u32 result

        let mut text_render_img =
            RgbaImage::from_pixel(text_width, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
        draw_text_mut(
            &mut text_render_img,
            text_color,
            0,
            text_y_offset - v_metrics.ascent.round() as i32, // Corrected y pos
            scale,
            font,
            &text,
        );

        if i > 0 {
            total_width = total_width.saturating_add(PADDING);
        }
        total_width = total_width.saturating_add(icon_width);
        total_width = total_width.saturating_add(PADDING);
        total_width = total_width.saturating_add(text_width);

        rendered_elements.push((icon_image_opt.cloned(), Some(text_render_img)));
    }

    if total_width == 0 {
        return Err("Calculated canvas width is zero".into());
    }

    let mut canvas = RgbaImage::from_pixel(total_width, ICON_HEIGHT, Rgba([0, 0, 0, 0]));
    let mut current_x: i64 = 0;

    for (i, (icon_opt, text_opt)) in rendered_elements.iter().enumerate() {
        if i > 0 {
            current_x += PADDING as i64;
        }
        if let Some(icon_img) = icon_opt {
            if current_x >= 0
                && current_x + (icon_img.width() as i64) <= canvas.width() as i64
                && icon_img.height() <= canvas.height()
            {
                image::imageops::overlay(&mut canvas, icon_img, current_x, 0);
            } else {
                eprintln!(
                    "Icon drawing position out of bounds: x={}, width={}",
                    current_x,
                    icon_img.width()
                );
            }
            current_x += icon_img.width() as i64;
        } else {
            current_x += (ICON_HEIGHT / 2) as i64;
        }
        current_x += PADDING as i64;
        if let Some(text_img) = text_opt {
            if current_x >= 0
                && current_x + (text_img.width() as i64) <= canvas.width() as i64
                && text_img.height() <= canvas.height()
            {
                image::imageops::overlay(&mut canvas, text_img, current_x, 0);
            } else {
                eprintln!(
                    "Text drawing position out of bounds: x={}, width={}",
                    current_x,
                    text_img.width()
                );
            }
            current_x += text_img.width() as i64;
        }
    }

    let rgba_data = canvas.into_raw();
    let final_icon = TrayIconImage::from_rgba(rgba_data, total_width, ICON_HEIGHT)?;
    let tooltip = tooltip_parts.join(" | ");
    Ok((final_icon, tooltip))
}

/// Creates a simple fallback icon with text when data loading/generation fails.
fn create_fallback_icon(font: &Arc<Font>, text: &str) -> TrayIconImage {
    let height = ICON_HEIGHT;
    let scale = Scale::uniform(height as f32 * 0.7);
    #[cfg(target_os = "macos")]
    let text_color = Rgba([255u8, 255u8, 255u8, 255u8]);
    #[cfg(not(target_os = "macos"))]
    // Use black text for fallback on non-macOS, as red might clash
    let text_color = Rgba([255u8, 255u8, 255u8, 255u8]); // White
    let bg_color = Rgba([0u8, 0u8, 0u8, 0u8]); // Transparent

    let glyphs: Vec<_> = font
        .layout(text, scale, rusttype::point(0.0, 0.0))
        .collect();
    let text_width_exact = glyphs
        .last()
        .map(|g| g.position().x + g.unpositioned().h_metrics().advance_width)
        .unwrap_or(50.0);

    let width = (text_width_exact.ceil() as u32).max(10) + PADDING * 2;
    let mut canvas = RgbaImage::from_pixel(width, height, bg_color);

    let v_metrics = font.v_metrics(scale);
    // Simpler vertical centering for fallback
    let text_y = ((height as f32 - (v_metrics.ascent - v_metrics.descent)) / 2.0 + v_metrics.ascent)
        .round() as i32;
    let text_x = ((width as f32 - text_width_exact) / 2.0).round() as i32;

    draw_text_mut(
        &mut canvas,
        text_color,
        text_x.max(0),
        text_y - v_metrics.descent.round() as i32, // Corrected y pos
        scale,
        font,
        text,
    );

    let rgba_data = canvas.into_raw();
    TrayIconImage::from_rgba(rgba_data, width, height).expect("Failed to create fallback icon")
}

// macOS specific redraw request (Keep as is)
#[cfg(target_os = "macos")]
fn request_macos_redraw() {
    use objc2_core_foundation::{CFRunLoopGetMain, CFRunLoopWakeUp};
    println!("Requesting macOS redraw...");
    unsafe {
        if let Some(rl) = CFRunLoopGetMain() {
            CFRunLoopWakeUp(&rl);
            // println!("CFRunLoopWakeUp called."); // Less verbose
        } else {
            eprintln!("Failed to get main CFRunLoop on macOS.");
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn request_macos_redraw() {
    // No-op
}
