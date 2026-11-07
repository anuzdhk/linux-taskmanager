mod actions;
mod app_icons;
mod bench;
mod bridge;
mod format;
mod processes;
mod settings;
mod system_pages;
mod tray;
mod updates;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use slint::{Model, ModelRc, SharedString, VecModel};
use taskmonitor_core::app_usage::{self, AppUsageLog};
use taskmonitor_core::services::{Bus, Service, Services};
use taskmonitor_core::users::{Logind, Session};
use taskmonitor_core::{GpuSampler, Process, ProcessSampler, Sampler, Snapshot};

slint::include_modules!();

/// Refresh intervals in ms, indexed like `refresh-labels` in shell.slint. 0 = paused.
const INTERVALS_MS: [u64; 5] = [500, 1000, 2000, 5000, 0];

/// The window was closed while running in the background. The sampler then only
/// feeds the top bar (and app history) instead of the hidden window.
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Brings the window back, whether it was minimised or closed to the background.
fn show_window(ui: &MainWindow) {
    WINDOW_HIDDEN.store(false, Ordering::Relaxed);
    ui.window().set_minimized(false);
    let _ = ui.show();
}

fn main() -> Result<(), slint::PlatformError> {
    if std::env::var_os("TASKMONITOR_BENCH").is_some() {
        bench::run();
        return Ok(());
    }
    // Launched again while a copy is running (perhaps in the background): show that one.
    if tray::activate_running() {
        return Ok(());
    }
    let ui = MainWindow::new()?;
    // Lets the desktop match the window to io.github.taskmonitor.desktop (name and icon).
    // Needs the platform that MainWindow::new() set up, and must happen before showing.
    slint::set_xdg_app_id("io.github.taskmonitor")?;
    ui.set_version(env!("CARGO_PKG_VERSION").into());
    ui.set_wayland(
        std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland"),
    );
    ui.on_open_url(|url| {
        let _ = std::process::Command::new("xdg-open").arg(url.as_str()).spawn();
    });

    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let initials: String = user.chars().take(2).collect::<String>().to_uppercase();
    ui.set_user_name(user.into());
    ui.set_user_initials(initials.into());

    let (tx, sampler_thread) = spawn_sampler(ui.as_weak());
    let quit = tx.clone();
    let send = move |cmd| {
        let _ = tx.send(cmd);
    };

    // Settings: theme, refresh rate, start page, always on top, app history.
    let prefs = settings::Settings::load();
    ui.set_theme_choice(prefs.theme);
    ui.set_refresh_index(prefs.refresh);
    ui.set_on_top(prefs.always_on_top);
    ui.set_app_history(prefs.app_history);
    ui.set_top_bar_stats(prefs.top_bar_stats);
    ui.set_run_in_background(prefs.run_in_background);
    ui.set_auto_update_check(prefs.check_updates);
    ui.set_zoom(prefs.zoom);
    ui.on_zoom_changed({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            apply_zoom(&ui);
            save_settings(&ui);
        }
    });
    ui.set_update_status(last_checked_text(settings::last_update_check()).into());
    ui.on_check_updates({
        let ui = ui.as_weak();
        move || check_for_updates(ui.clone(), true)
    });
    // Automatic check: once a day, a few seconds after start so it never slows startup.
    let now = unix_now();
    if prefs.check_updates && now.saturating_sub(settings::last_update_check()) >= updates::INTERVAL_SECS {
        let ui = ui.as_weak();
        slint::Timer::single_shot(Duration::from_secs(8), move || check_for_updates(ui, false));
    }
    if prefs.top_bar_stats {
        send(Cmd::TopBar(true));
    }
    ui.on_top_bar_toggled({
        let send = send.clone();
        let ui = ui.as_weak();
        move |on| {
            let ui = ui.unwrap();
            // Running in the background without the top-bar item would leave no way back in.
            if !on {
                ui.set_run_in_background(false);
            }
            save_settings(&ui);
            send(Cmd::TopBar(on));
        }
    });
    ui.on_background_toggled({
        let send = send.clone();
        let ui = ui.as_weak();
        move |on| {
            let ui = ui.unwrap();
            if on && !ui.get_top_bar_stats() {
                ui.set_top_bar_stats(true);
                send(Cmd::TopBar(true));
            }
            save_settings(&ui);
        }
    });
    // Closing hides the window when running in the background; the top-bar item
    // brings it back and its menu quits. Otherwise closing quits as usual.
    ui.window().on_close_requested({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            if ui.get_run_in_background() && ui.get_top_bar_stats() {
                WINDOW_HIDDEN.store(true, Ordering::Relaxed);
            } else {
                let _ = slint::quit_event_loop();
            }
            slint::CloseRequestResponse::HideWindow
        }
    });
    let start = match prefs.start_page.as_str() {
        "performance" => (1, Page::Performance),
        "details" => (2, Page::Details),
        _ => (0, Page::Processes),
    };
    ui.set_start_page(start.0);
    ui.set_page(start.1);
    send(Cmd::Page(start.1));
    if prefs.refresh != settings::Settings::default().refresh {
        send(Cmd::Interval(INTERVALS_MS[prefs.refresh as usize]));
    }
    send(Cmd::History(prefs.app_history));
    ui.on_settings_changed({
        let send = send.clone();
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            save_settings(&ui);
            send(Cmd::Interval(INTERVALS_MS.get(ui.get_refresh_index() as usize).copied().unwrap_or(1000)));
            send(Cmd::History(ui.get_app_history()));
        }
    });
    ui.on_clear_history({
        let send = send.clone();
        move || send(Cmd::ClearHistory)
    });
    ui.on_refresh_changed({
        let send = send.clone();
        let ui = ui.as_weak();
        move |i| {
            save_settings(&ui.unwrap());
            send(Cmd::Interval(INTERVALS_MS.get(i as usize).copied().unwrap_or(1000)))
        }
    });
    ui.on_page_changed({
        let send = send.clone();
        move |p| send(Cmd::Page(p))
    });
    ui.on_perf_tab_changed({
        let send = send.clone();
        move |t| send(Cmd::PerfTab(t))
    });
    ui.on_sort({
        let send = send.clone();
        move |c| send(Cmd::Sort(c))
    });
    ui.on_toggle({
        let send = send.clone();
        move |k| send(Cmd::Toggle(k))
    });
    ui.on_filter_changed({
        let send = send.clone();
        move |f| send(Cmd::Filter(f))
    });
    ui.on_action({
        let send = send.clone();
        move |a, k| send(Cmd::Action(a, k))
    });
    ui.on_service_bus_changed({
        let send = send.clone();
        move |user| send(Cmd::ServiceBus(user))
    });
    ui.on_service_act({
        let send = send.clone();
        move |verb, name| send(Cmd::ServiceAct(verb, name))
    });
    ui.on_startup_toggled({
        let send = send.clone();
        move |id, on| send(Cmd::StartupToggle(id, on))
    });
    ui.on_session_act({
        let send = send.clone();
        move |verb, key| send(Cmd::SessionAct(verb, key))
    });
    ui.on_properties({
        let send = send.clone();
        move |k| send(Cmd::Properties(k))
    });
    ui.on_affinity_query({
        let send = send.clone();
        move |k| send(Cmd::AffinityQuery(k))
    });
    ui.on_affinity_all({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            let all = vec![true; ui.get_affinity_cpus().row_count()];
            ui.set_affinity_cpus(ModelRc::new(VecModel::from(all)));
        }
    });
    ui.on_affinity_apply({
        let ui = ui.as_weak();
        move |k| {
            let mask: String = ui.unwrap().get_affinity_cpus().iter().map(|on| if on { '1' } else { '0' }).collect();
            if mask.contains('1') {
                send(Cmd::Action(format!("affinity:{mask}").into(), k));
            } else {
                ui.unwrap().set_toast("Pick at least one processor".into());
            }
        }
    });
    ui.on_run_command({
        let ui = ui.as_weak();
        move |cmd| {
            let cmd = cmd.trim();
            if cmd.is_empty() {
                return;
            }
            let msg = match taskmonitor_core::actions::run(cmd) {
                Ok(pid) => format!("Started {cmd} (PID {pid})"),
                Err(e) => format!("Couldn't start {cmd}: {e}"),
            };
            ui.unwrap().set_toast(msg.into());
        }
    });
    if let Ok(page) = std::env::var("TASKMONITOR_PAGE") {
        let page = match page.as_str() {
            "processes" => Page::Processes,
            "details" => Page::Details,
            "services" => Page::Services,
            "startup" => Page::Startup,
            "users" => Page::Users,
            "history" => Page::History,
            "settings" => Page::Settings,
            _ => Page::Performance,
        };
        ui.set_page(page);
        ui.invoke_page_changed(page);
    }
    // Dev aid: TASKMONITOR_SORT=mem sorts the tables by that column (for screenshots).
    if let Ok(column) = std::env::var("TASKMONITOR_SORT") {
        ui.invoke_sort(column.into());
    }
    if let Some(tab) = std::env::var("TASKMONITOR_TAB").ok().and_then(|t| t.parse().ok()) {
        ui.set_perf_tab(tab);
        ui.invoke_perf_tab_changed(tab);
    }

    // Dev aid: TASKMONITOR_SNAPSHOT=out.ppm saves the rendered window and quits.
    // Wayland compositors block outside screenshots, so this is how we check the UI.
    if let Ok(path) = std::env::var("TASKMONITOR_SNAPSHOT") {
        if let Ok(theme) = std::env::var("TASKMONITOR_THEME") {
            ui.global::<Theme>().set_dark(theme == "dark");
        }
        // TASKMONITOR_OPEN=properties|affinity opens that dialog for TaskMonitor itself.
        let own = SharedString::from(format!("pid:{}", std::process::id()));
        match std::env::var("TASKMONITOR_OPEN").as_deref() {
            Ok("properties") => ui.invoke_properties(own),
            Ok("affinity") => ui.invoke_affinity_query(own),
            _ => {}
        }
        let delay = std::env::var("TASKMONITOR_SNAPSHOT_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(800);
        let weak = ui.as_weak();
        slint::Timer::single_shot(Duration::from_millis(delay), move || {
            let ui = weak.unwrap();
            match ui.window().take_snapshot() {
                Ok(img) => {
                    let mut ppm = format!("P6 {} {} 255\n", img.width(), img.height()).into_bytes();
                    ppm.extend(img.as_slice().iter().flat_map(|p| [p.r, p.g, p.b]));
                    std::fs::write(&path, ppm).expect("write snapshot");
                }
                Err(e) => eprintln!("snapshot failed: {e}"),
            }
            let _ = slint::quit_event_loop();
        });
    }

    ui.show()?;
    // The window only exists once the event loop runs; apply a saved zoom right after.
    slint::Timer::single_shot(Duration::from_millis(100), {
        let ui = ui.as_weak();
        move || apply_zoom(&ui.unwrap())
    });
    slint::run_event_loop_until_quit()?;
    // Let the sampler save app history before the process exits.
    let _ = quit.send(Cmd::Quit);
    let _ = sampler_thread.join();
    Ok(())
}

thread_local! {
    /// (desktop scale factor, scale factor we last set).
    static SCALE: std::cell::Cell<(f32, f32)> = const { std::cell::Cell::new((1.0, 0.0)) };
}

/// Zooms the whole UI by scaling Slint's scale factor on top of the desktop's. Called on
/// every UI update too: the window is created (and the desktop scale can change, say on
/// another monitor) after start, and either resets the factor to the desktop's.
fn apply_zoom(ui: &MainWindow) {
    use slint::platform::WindowEvent;
    let window = ui.window();
    let current = window.scale_factor();
    let (mut desktop, set) = SCALE.get();
    if current != set {
        desktop = current;
    }
    let factor = desktop * ui.get_zoom() as f32 / 100.0;
    if factor == current {
        return;
    }
    SCALE.set((desktop, factor));
    window.dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor: factor });
    // Same physical window, so the logical size changes with the factor.
    window.dispatch_event(WindowEvent::Resized { size: window.size().to_logical(factor) });
}

fn save_settings(ui: &MainWindow) {
    let prefs = settings::Settings {
        theme: ui.get_theme_choice(),
        refresh: ui.get_refresh_index(),
        start_page: ["processes", "performance", "details"][ui.get_start_page().clamp(0, 2) as usize].into(),
        always_on_top: ui.get_on_top(),
        app_history: ui.get_app_history(),
        top_bar_stats: ui.get_top_bar_stats(),
        run_in_background: ui.get_run_in_background(),
        check_updates: ui.get_auto_update_check(),
        zoom: ui.get_zoom(),
    };
    if let Err(e) = prefs.save() {
        ui.set_toast(format!("Couldn't save settings: {e}").into());
    }
}

/// Messages from the UI to the sampler thread.
enum Cmd {
    /// Refresh interval in ms; 0 pauses.
    Interval(u64),
    Page(Page),
    /// Performance page tab: 0 disk, 1 network, 2 GPU.
    PerfTab(i32),
    Sort(SharedString),
    Filter(SharedString),
    Toggle(SharedString),
    /// (action name, row key), see actions::Action::parse.
    Action(SharedString, SharedString),
    Properties(SharedString),
    AffinityQuery(SharedString),
    /// Services page: false = system services, true = the user's.
    ServiceBus(bool),
    /// (start|stop|restart|enable|disable, unit name without .service)
    ServiceAct(SharedString, SharedString),
    /// A service action finished; reload unit-file states.
    ServicesChanged,
    StartupToggle(SharedString, bool),
    /// (lock|terminate, session id)
    SessionAct(SharedString, SharedString),
    History(bool),
    ClearHistory,
    /// Show (true) or remove CPU / RAM / GPU in the desktop's top bar.
    TopBar(bool),
    Quit,
}

/// Samples on a background thread and pushes updates into the UI.
/// Per-process data is only collected while a page that shows it is open.
fn spawn_sampler(ui: slint::Weak<MainWindow>) -> (mpsc::Sender<Cmd>, std::thread::JoinHandle<()>) {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let tx_back = tx.clone();
    let handle = std::thread::Builder::new()
        .name("sampler".into())
        .spawn(move || {
            let mut sampler = Sampler::new();
            let mut procs_sampler: Option<ProcessSampler> = None;
            let mut histories = bridge::Histories::default();
            let mut view = processes::View::default();
            let mut gpu_sampler = GpuSampler::new();
            let mut page = Page::Performance;
            let mut perf_tab = 0;
            let mut last: (Snapshot, Vec<Process>) = Default::default();
            let mut gpu_total = 0.0f32;
            let mut services: Option<Services> = None;
            let mut service_bus = Bus::System;
            let mut service_list: Vec<Service> = Vec::new();
            let mut logind: Option<Logind> = None;
            let history_path = app_usage::default_path();
            let mut usage_log: Option<AppUsageLog> = None;
            let mut last_save = std::time::Instant::now();
            let mut tick: u64 = 0;
            let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
            let mut interval = Some(Duration::from_millis(1000));
            // The first tick comes quickly so the window doesn't sit on placeholders.
            let mut wait = Some(Duration::from_millis(250));

            let mut icons = app_icons::IconStore::new();
            let mut registry: Option<taskmonitor_core::apps::Registry> = None;
            let mut top_bar: Option<tray::Tray> = None;
            let push_rows = |view: &processes::View,
                             page: Page,
                             last: &(Snapshot, Vec<Process>),
                             gpu_total: f32,
                             icons: &mut app_icons::IconStore| {
                let rows = view.build(page, &last.1, &last.0, gpu_total, icons);
                let new_icons = icons.take_pending();
                ui.upgrade_in_event_loop(move |ui| {
                    app_icons::apply(&ui, new_icons);
                    processes::apply(&ui, rows);
                })
                .is_ok()
            };

            loop {
                let msg = match wait {
                    Some(d) => rx.recv_timeout(d),
                    None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
                };
                let shows_procs = |p: Page| matches!(p, Page::Processes | Page::Details | Page::Users);
                match msg {
                    Ok(Cmd::Interval(ms)) => {
                        let resumed = interval.is_none() && ms > 0;
                        interval = (ms > 0).then(|| Duration::from_millis(ms));
                        wait = interval;
                        if resumed {
                            // Reset the baseline so the first rate after a pause isn't averaged over it.
                            sampler.sample();
                            if let Some(p) = procs_sampler.as_mut() {
                                p.sample();
                            }
                        }
                        continue;
                    }
                    Ok(cmd) => {
                        match cmd {
                            Cmd::Page(p) => {
                                page = p;
                                if p == Page::Startup {
                                    push_startup(&ui, &mut icons);
                                }
                                if p == Page::Services {
                                    let svc = services.get_or_insert_with(Services::new);
                                    service_list = push_services(&ui, svc, service_bus, &view.filter);
                                }
                                // Sample right away so the table isn't empty for a whole interval.
                                if shows_procs(p) {
                                    let ps = procs_sampler.get_or_insert_with(ProcessSampler::new);
                                    last.1 = ps.sample();
                                }
                            }
                            Cmd::PerfTab(t) => {
                                perf_tab = t;
                                continue;
                            }
                            Cmd::Sort(c) => view.set_sort(c),
                            Cmd::Filter(f) => {
                                view.filter = f.to_string();
                                if page == Page::Services {
                                    let (rows, summary) = system_pages::service_rows(&service_list, &view.filter);
                                    let _ = ui.upgrade_in_event_loop(move |ui| {
                                        bridge::sync(ui.get_service_rows(), rows, |m| ui.set_service_rows(m));
                                        ui.set_service_summary(summary.into());
                                    });
                                    continue;
                                }
                            }
                            Cmd::ServiceBus(user) => {
                                service_bus = if user { Bus::User } else { Bus::System };
                                let svc = services.get_or_insert_with(Services::new);
                                service_list = push_services(&ui, svc, service_bus, &view.filter);
                                continue;
                            }
                            Cmd::ServiceAct(verb, name) => {
                                service_action(&ui, &tx_back, service_bus, &verb, &name);
                                continue;
                            }
                            Cmd::ServicesChanged => {
                                if let Some(svc) = services.as_mut() {
                                    // Our own connection's cache is stale after another connection changed units.
                                    *svc = Services::new();
                                    service_list = push_services(&ui, svc, service_bus, &view.filter);
                                }
                                continue;
                            }
                            Cmd::StartupToggle(id, on) => {
                                let msg = match taskmonitor_core::startup::set_enabled(&id, on) {
                                    Ok(()) => None,
                                    Err(e) => Some(format!("Couldn't change startup setting: {e}")),
                                };
                                push_startup(&ui, &mut icons);
                                if let Some(msg) = msg {
                                    let _ = ui.upgrade_in_event_loop(move |ui| ui.set_toast(msg.into()));
                                }
                                continue;
                            }
                            Cmd::SessionAct(verb, key) => {
                                session_action(&ui, &verb, &key);
                                continue;
                            }
                            Cmd::History(on) => {
                                if on && usage_log.is_none() {
                                    usage_log = Some(AppUsageLog::load(&history_path));
                                } else if !on && let Some(log) = usage_log.take() {
                                    let _ = log.save(&history_path);
                                }
                                push_history(&ui, usage_log.as_ref(), &mut icons, &mut registry);
                                continue;
                            }
                            Cmd::ClearHistory => {
                                if usage_log.is_some() {
                                    let log = AppUsageLog::default();
                                    let _ = log.save(&history_path);
                                    usage_log = Some(log);
                                }
                                push_history(&ui, usage_log.as_ref(), &mut icons, &mut registry);
                                continue;
                            }
                            Cmd::TopBar(on) => {
                                if !on {
                                    top_bar = None;
                                } else if top_bar.is_none() {
                                    match tray::Tray::start(ui.clone()) {
                                        Ok(t) => top_bar = Some(t),
                                        Err(e) => {
                                            let msg = format!("Can't show stats in the top bar: {e}");
                                            let _ = ui.upgrade_in_event_loop(move |ui| {
                                                ui.set_top_bar_stats(false);
                                                ui.set_run_in_background(false);
                                                ui.set_toast(msg.into());
                                            });
                                        }
                                    }
                                }
                                continue;
                            }
                            Cmd::Quit => {
                                if let Some(log) = &usage_log {
                                    let _ = log.save(&history_path);
                                }
                                break;
                            }
                            Cmd::Toggle(k) => view.toggle(k),
                            Cmd::Action(a, k) => {
                                run_action(&ui, &a, &k, last.1.clone());
                                continue;
                            }
                            Cmd::Properties(k) => {
                                show_properties(&ui, &k, &last.1);
                                continue;
                            }
                            Cmd::AffinityQuery(k) => {
                                show_affinity(&ui, &k, &last.1);
                                continue;
                            }
                            Cmd::Interval(_) => unreachable!(),
                        }
                        if shows_procs(page) && !push_rows(&view, page, &last, gpu_total, &mut icons) {
                            break;
                        }
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                wait = interval;
                let snap = sampler.sample();
                // NVIDIA is only queried where its numbers are shown (see gpu.rs on why).
                let gpu_detail = page == Page::Processes || (page == Page::Performance && perf_tab == 2);
                let (gpus, gpu_per_pid) = gpu_sampler.sample(gpu_detail);
                gpu_total = gpus.iter().filter_map(|g| g.usage).fold(0.0, f32::max);
                let update = bridge::build(&snap, &gpus, &mut histories);
                if let Some(t) = top_bar.as_mut() {
                    let gpu = gpus.iter().filter_map(|g| g.usage).reduce(f32::max);
                    t.set_label(tray::label(snap.cpu_usage, snap.memory.used(), gpu));
                }
                let hidden = WINDOW_HIDDEN.load(Ordering::Relaxed);
                if !hidden && ui.upgrade_in_event_loop(move |ui| {
                    apply_zoom(&ui);
                    bridge::apply(&ui, update)
                }).is_err() {
                    break; // window closed
                }
                last.0 = snap;
                tick += 1;
                // App history needs process samples even when no process page is open,
                // but every 5th tick is enough: it accumulates rates times elapsed time.
                // The Performance page's Disk activity card needs processes too; every
                // 2nd tick is plenty for it (rates average over the time between samples).
                let sample_procs = (!hidden
                    && (shows_procs(page) || (page == Page::Performance && tick.is_multiple_of(2))))
                    || (usage_log.is_some() && tick.is_multiple_of(5));
                if sample_procs {
                    let ps = procs_sampler.get_or_insert_with(ProcessSampler::new);
                    last.1 = ps.sample();
                    for p in &mut last.1 {
                        p.gpu = gpu_per_pid.get(&p.pid).copied().unwrap_or(0.0);
                    }
                    if let Some(log) = usage_log.as_mut() {
                        log.record(&last.1, ps.last_interval, cpus);
                    }
                }
                if let Some(log) = &usage_log
                    && last_save.elapsed() > Duration::from_secs(60)
                {
                    let _ = log.save(&history_path);
                    last_save = std::time::Instant::now();
                }
                if hidden {
                    continue;
                }
                if sample_procs && page == Page::Performance {
                    let rows = processes::disk_rows(&last.1, &mut icons);
                    let new_icons = icons.take_pending();
                    let _ = ui.upgrade_in_event_loop(move |ui| {
                        app_icons::apply(&ui, new_icons);
                        bridge::sync(ui.get_disk_procs(), rows, |m| ui.set_disk_procs(m));
                    });
                }
                if page == Page::History {
                    push_history(&ui, usage_log.as_ref(), &mut icons, &mut registry);
                }
                if shows_procs(page) && !push_rows(&view, page, &last, gpu_total, &mut icons) {
                    break;
                }
                if page == Page::Services {
                    let svc = services.get_or_insert_with(Services::new);
                    service_list = push_services(&ui, svc, service_bus, &view.filter);
                }
                if page == Page::Users {
                    let sessions: Vec<Session> = logind.get_or_insert_with(Logind::new).sessions().unwrap_or_default();
                    let rows = system_pages::user_rows(&sessions, &last.1, &last.0);
                    let _ = ui.upgrade_in_event_loop(move |ui| bridge::sync(ui.get_user_rows(), rows, |m| ui.set_user_rows(m)));
                }
            }
        })
        .expect("spawn sampler thread");
    (tx, handle)
}

/// Runs an action on its own thread: a polkit password prompt may block for a while.
fn run_action(ui: &slint::Weak<MainWindow>, action: &str, key: &str, procs: Vec<Process>) {
    let Some(action) = actions::Action::parse(action) else { return };
    let (ui, key) = (ui.clone(), key.to_owned());
    std::thread::spawn(move || {
        let msg = actions::perform(&action, &key, &procs);
        let _ = ui.upgrade_in_event_loop(move |ui| ui.set_toast(msg.into()));
    });
}

fn show_properties(ui: &slint::Weak<MainWindow>, key: &str, procs: &[Process]) {
    let Some(&pid) = actions::pids_for_key(key, procs).first() else { return };
    let title = format!("{} (PID {pid})", actions::name_for_key(&format!("pid:{pid}"), procs));
    let info = taskmonitor_core::actions::info(pid);
    let all_text: String =
        std::iter::once(title.clone()).chain(info.iter().map(|(l, v)| format!("{l}: {v}"))).collect::<Vec<_>>().join("\n");
    let facts: Vec<Metric> = info
        .into_iter()
        .map(|(label, value)| Metric { label: label.into(), value: value.into(), unit: "".into() })
        .collect();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        ui.set_props_text(all_text.into());
        ui.set_props_title(title.into());
        ui.set_props(ModelRc::new(VecModel::from(facts)));
        ui.set_props_open(true);
    });
}

fn show_affinity(ui: &slint::Weak<MainWindow>, key: &str, procs: &[Process]) {
    let Some(&pid) = actions::pids_for_key(key, procs).first() else { return };
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    let title = actions::name_for_key(key, procs);
    let result = taskmonitor_core::actions::affinity(pid, cpus);
    let _ = ui.upgrade_in_event_loop(move |ui| match result {
        Ok(mask) => {
            ui.set_affinity_title(title.into());
            ui.set_affinity_cpus(ModelRc::new(VecModel::from(mask)));
            ui.set_affinity_open(true);
        }
        Err(e) => ui.set_toast(format!("Can't read affinity of {title}: {}", e.desc()).into()),
    });
}

fn push_services(ui: &slint::Weak<MainWindow>, svc: &Services, bus: Bus, filter: &str) -> Vec<Service> {
    let list = match svc.list(bus) {
        Ok(list) => list,
        Err(e) => {
            let msg = format!("Can't read services: {e}");
            let _ = ui.upgrade_in_event_loop(move |ui| ui.set_service_summary(msg.into()));
            return Vec::new();
        }
    };
    let (rows, summary) = system_pages::service_rows(&list, filter);
    let _ = ui.upgrade_in_event_loop(move |ui| {
        bridge::sync(ui.get_service_rows(), rows, |m| ui.set_service_rows(m));
        ui.set_service_summary(summary.into());
    });
    list
}

fn push_startup(ui: &slint::Weak<MainWindow>, icons: &mut app_icons::IconStore) {
    let rows = system_pages::startup_rows(&taskmonitor_core::startup::list(), icons);
    let new_icons = icons.take_pending();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        app_icons::apply(&ui, new_icons);
        bridge::sync(ui.get_startup_rows(), rows, |m| ui.set_startup_rows(m));
    });
}

/// systemd may show a password prompt, so this runs on its own thread with its own connection.
fn service_action(ui: &slint::Weak<MainWindow>, done: &mpsc::Sender<Cmd>, bus: Bus, verb: &str, name: &str) {
    let (ui, done, verb, unit) = (ui.clone(), done.clone(), verb.to_owned(), format!("{name}.service"));
    std::thread::spawn(move || {
        let msg = match Services::new().control(bus, &verb, &unit) {
            Ok(()) => {
                let past = match verb.as_str() {
                    "start" => "Started",
                    "stop" => "Stopped",
                    "restart" => "Restarted",
                    "enable" => "Enabled",
                    _ => "Disabled",
                };
                format!("{past} {unit}")
            }
            Err(e) => format!("Couldn't {verb} {unit}: {}", dbus_error(&e)),
        };
        let _ = done.send(Cmd::ServicesChanged);
        let _ = ui.upgrade_in_event_loop(move |ui| ui.set_toast(msg.into()));
    });
}

fn session_action(ui: &slint::Weak<MainWindow>, verb: &str, session: &str) {
    let (ui, verb, session) = (ui.clone(), verb.to_owned(), session.to_owned());
    std::thread::spawn(move || {
        let msg = match Logind::new().control(&verb, &session) {
            Ok(()) if verb == "lock" => format!("Locked session {session}"),
            Ok(()) => format!("Signing out session {session}"),
            Err(e) => format!("Couldn't change session {session}: {}", dbus_error(&e)),
        };
        let _ = ui.upgrade_in_event_loop(move |ui| ui.set_toast(msg.into()));
    });
}

/// The human part of a D-Bus error ("Access denied" rather than the error name).
fn dbus_error(e: &zbus::Error) -> String {
    match e {
        zbus::Error::MethodError(_, Some(detail), _) => detail.clone(),
        other => other.to_string(),
    }
}

fn push_history(
    ui: &slint::Weak<MainWindow>,
    log: Option<&AppUsageLog>,
    icons: &mut app_icons::IconStore,
    registry: &mut Option<taskmonitor_core::apps::Registry>,
) {
    let (rows, since) = match log {
        Some(l) => {
            let apps = registry.get_or_insert_with(taskmonitor_core::apps::Registry::load);
            (system_pages::history_rows(l, icons, apps), system_pages::since_text(l.since))
        }
        None => Default::default(),
    };
    let new_icons = icons.take_pending();
    let _ = ui.upgrade_in_event_loop(move |ui| {
        app_icons::apply(&ui, new_icons);
        bridge::sync(ui.get_history_rows(), rows, |m| ui.set_history_rows(m));
        ui.set_history_since(since.into());
    });
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn last_checked_text(when: u64) -> String {
    if when == 0 {
        return "Not checked yet.".into();
    }
    let ago = unix_now().saturating_sub(when);
    let ago = match ago {
        0..120 => "just now".to_owned(),
        120..7200 => format!("{} minutes ago", ago / 60),
        7200..172_800 => format!("{} hours ago", ago / 3600),
        _ => format!("{} days ago", ago / 86_400),
    };
    format!("You're up to date. Last checked {ago}.")
}

/// Runs the update check on its own thread (it waits on the network). `manual` checks
/// report every outcome; automatic ones stay quiet unless there's an update.
fn check_for_updates(ui: slint::Weak<MainWindow>, manual: bool) {
    let _ = ui.upgrade_in_event_loop(|ui| ui.set_checking_updates(true));
    std::thread::spawn(move || {
        // Dev aid: TASKMONITOR_PRETEND_VERSION=0.0.1 shows the "update available" state.
        let current = std::env::var("TASKMONITOR_PRETEND_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").into());
        let result = updates::check(&current);
        if result.is_ok() {
            settings::set_last_update_check(unix_now());
        }
        let _ = ui.upgrade_in_event_loop(move |ui| {
            ui.set_checking_updates(false);
            match result {
                Ok(updates::Outcome::Available(release)) => {
                    ui.set_update_status(
                        "Download the package for your system from the release page, then install it as before.".into(),
                    );
                    ui.set_update_version(release.version.as_str().into());
                    ui.set_update_url(release.url.into());
                    ui.set_toast(format!("TaskMonitor {} is available", release.version).into());
                }
                Ok(updates::Outcome::UpToDate) => {
                    ui.set_update_status(last_checked_text(unix_now()).into());
                    if manual {
                        ui.set_toast("TaskMonitor is up to date".into());
                    }
                }
                Err(e) => {
                    ui.set_update_status(format!("Couldn't check for updates: {e}.").into());
                    if manual {
                        ui.set_toast(format!("Couldn't check for updates: {e}").into());
                    }
                }
            }
        });
    });
}
