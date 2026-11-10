//! The main window: saved computers, their online status, LAN scan and pairing.

use crate::net;
use crate::store::{self, Host};
use crate::wake;
use anyhow::{anyhow, Result};
use eframe::egui;
use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Candidate {
    name: String,
    addr: String,
    fingerprint: String,
}

#[derive(Default)]
struct Shared {
    online: HashMap<String, bool>,
    /// Network card addresses found by the status checks, by computer address.
    macs: HashMap<String, String>,
    /// Addresses whose card address is already saved, so they aren't looked up again.
    known_macs: Vec<String>,
    watched: Vec<String>,
    found: Vec<Candidate>,
    to_confirm: Option<Candidate>,
    message: String,
    scanning: bool,
}

struct Dashboard {
    hosts: Vec<Host>,
    public_key: String,
    new_addr: String,
    shared: Arc<Mutex<Shared>>,
    /// When "Copy command" was last clicked, and whether it worked.
    copied: Option<(Instant, bool)>,
    /// The row being renamed, and the name typed so far.
    renaming: Option<(usize, String)>,
}

/// `.\` makes PowerShell run the file from the current folder, and cmd accepts it too.
fn install_command(public_key: &str) -> String {
    format!(".\\rdagent.exe install --admin-key {public_key}")
}

pub fn run() -> Result<()> {
    let seed = store::load_or_create_key()?;
    let hosts = store::load_hosts();
    let shared = Arc::new(Mutex::new(Shared {
        watched: hosts.iter().map(|h| h.addr.clone()).collect(),
        known_macs: hosts.iter().filter(|h| h.mac.is_some()).map(|h| h.addr.clone()).collect(),
        ..Default::default()
    }));
    start_status_checks(shared.clone());
    let app = Dashboard {
        hosts,
        public_key: hex::encode(rd_common::auth::public_key(&seed)),
        new_addr: String::new(),
        shared,
        copied: None,
        renaming: None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([820.0, 560.0]).with_title("Remote Access"),
        ..Default::default()
    };
    eframe::run_native("Remote Access", options, Box::new(|_| Ok(Box::new(app))))
        .map_err(|e| anyhow!("couldn't open the dashboard: {e}"))
}

fn start_status_checks(shared: Arc<Mutex<Shared>>) {
    std::thread::spawn(move || loop {
        let watched = shared.lock().unwrap().watched.clone();
        let results: Vec<_> = watched
            .into_iter()
            .map(|addr| {
                let up = addr
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut a| a.next())
                    .is_some_and(|a| TcpStream::connect_timeout(&a, Duration::from_millis(800)).is_ok());
                (addr, up)
            })
            .collect();
        // Online computers are in the local address table now, so learn their card address.
        let known = shared.lock().unwrap().known_macs.clone();
        let macs: Vec<_> = results
            .iter()
            .filter(|(addr, up)| *up && !known.contains(addr))
            .filter_map(|(addr, _)| {
                let mac = wake::lookup(wake::ipv4_of(addr)?)?;
                Some((addr.clone(), wake::format_mac(&mac)))
            })
            .collect();
        let mut s = shared.lock().unwrap();
        s.online.extend(results);
        s.macs.extend(macs);
        drop(s);
        std::thread::sleep(Duration::from_secs(5));
    });
}

fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().ok()?;
    Some(runtime.block_on(future))
}

fn start_scan(shared: Arc<Mutex<Shared>>) {
    {
        let mut s = shared.lock().unwrap();
        s.scanning = true;
        s.message.clear();
    }
    std::thread::spawn(move || {
        let result = block_on(net::discover(Duration::from_millis(1500)));
        let mut s = shared.lock().unwrap();
        s.scanning = false;
        match result {
            Some(Ok(found)) => {
                if found.is_empty() {
                    s.message = "No computers answered. You can add one by its IP address.".to_string();
                }
                s.found = found
                    .into_iter()
                    .map(|(addr, reply)| Candidate { name: reply.name, addr, fingerprint: reply.fingerprint })
                    .collect();
            }
            Some(Err(e)) => s.message = format!("Scan failed: {e}"),
            None => s.message = "Scan failed.".to_string(),
        }
    });
}

/// Reads identity directly from the computer, because a discovery reply is unauthenticated.
fn start_probe(shared: Arc<Mutex<Shared>>, addr: String) {
    std::thread::spawn(move || {
        let result = block_on(net::probe(&addr));
        let mut s = shared.lock().unwrap();
        match result {
            Some(Ok((name, fingerprint))) => {
                s.found.retain(|c| c.addr != addr);
                s.to_confirm = Some(Candidate { name, addr, fingerprint });
            }
            Some(Err(e)) => s.message = format!("Couldn't reach {addr}: {e}"),
            None => s.message = format!("Couldn't reach {addr}."),
        }
    });
}

/// Sends the wake packet. Returns a message to show.
fn send_wake(host: &Host) -> String {
    let Some(mac) = host.mac.as_deref().and_then(wake::parse_mac) else {
        return format!("{} can't be woken yet: it needs to be seen online once first.", host.name);
    };
    match wake::wake(&mac, wake::ipv4_of(&host.addr)) {
        Ok(()) => format!("Sent a wake signal to {}. It can take up to a minute to come online.", host.name),
        Err(e) => format!("Couldn't send a wake signal to {}: {e}", host.name),
    }
}

fn open_session(host: &Host) -> Result<()> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.args(["session", "--addr", &host.addr, "--fingerprint", &host.fingerprint, "--name", &host.name]);
    if let Some(mac) = &host.mac {
        command.args(["--mac", mac]);
    }
    command.spawn()?;
    Ok(())
}

impl Dashboard {
    /// Saves card addresses the status checks have learned.
    fn record_macs(&mut self) {
        let macs = std::mem::take(&mut self.shared.lock().unwrap().macs);
        let mut changed = false;
        for (addr, mac) in macs {
            for host in self.hosts.iter_mut().filter(|h| h.addr == addr) {
                if host.mac.as_deref() != Some(mac.as_str()) {
                    host.mac = Some(mac.clone());
                    changed = true;
                }
            }
        }
        if changed {
            self.save();
        }
    }

    fn save(&mut self) {
        {
            let mut s = self.shared.lock().unwrap();
            s.watched = self.hosts.iter().map(|h| h.addr.clone()).collect();
            s.known_macs = self.hosts.iter().filter(|h| h.mac.is_some()).map(|h| h.addr.clone()).collect();
        }
        if let Err(e) = store::save_hosts(&self.hosts) {
            self.shared.lock().unwrap().message = format!("Couldn't save the computer list: {e}");
        }
    }

    fn computers(&mut self, ui: &mut egui::Ui) {
        if self.hosts.is_empty() {
            ui.label("No computers yet. Install the agent on one, then scan the network or add it by address.");
            return;
        }
        let online = self.shared.lock().unwrap().online.clone();
        let mut remove = None;
        let mut failure = None;
        let mut notice = None;
        let mut rename_done = None::<bool>;
        egui::Grid::new("computers").striped(true).num_columns(7).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.strong("Computer");
            ui.strong("Address");
            ui.strong("Status");
            ui.label("");
            ui.label("");
            ui.label("");
            ui.label("");
            ui.end_row();
            for (i, host) in self.hosts.iter().enumerate() {
                match &mut self.renaming {
                    Some((index, draft)) if *index == i => {
                        let edit = ui.add(egui::TextEdit::singleline(draft).desired_width(180.0));
                        if !edit.has_focus() && !edit.lost_focus() {
                            edit.request_focus();
                        }
                        if edit.lost_focus() {
                            // Esc cancels. Enter, or clicking elsewhere, saves.
                            rename_done = Some(!ui.input(|input| input.key_pressed(egui::Key::Escape)));
                        }
                    }
                    _ => {
                        ui.label(&host.name);
                    }
                }
                ui.label(&host.addr);
                match online.get(&host.addr) {
                    Some(true) => ui.colored_label(egui::Color32::from_rgb(60, 170, 90), "Online"),
                    Some(false) => ui.colored_label(egui::Color32::from_rgb(200, 80, 70), "Offline"),
                    None => ui.label("Checking…"),
                };
                let offline = online.get(&host.addr) == Some(&false);
                if ui.button("Connect").clicked() {
                    // The session window keeps retrying, so it connects once the computer wakes.
                    if offline && host.mac.is_some() {
                        notice = Some(send_wake(host));
                    }
                    if let Err(e) = open_session(host) {
                        failure = Some(format!("Couldn't open a session window: {e}"));
                    }
                }
                let wake_button = egui::Button::new("Wake");
                let wake_hint = if host.mac.is_some() {
                    "Wake this computer from sleep over the network (needs a network cable)"
                } else {
                    "Available once this computer has been seen online"
                };
                if ui.add_enabled(host.mac.is_some(), wake_button).on_hover_text(wake_hint).on_disabled_hover_text(wake_hint).clicked() {
                    notice = Some(send_wake(host));
                }
                let renaming_this = self.renaming.as_ref().is_some_and(|(index, _)| *index == i);
                if renaming_this {
                    ui.weak("Enter to save");
                    ui.weak("Esc to cancel");
                } else {
                    if ui.button("Rename").clicked() {
                        self.renaming = Some((i, host.name.clone()));
                    }
                    if ui.button("Remove").clicked() {
                        remove = Some(i);
                    }
                }
                ui.end_row();
            }
        });
        if let Some(save) = rename_done {
            if let Some((index, draft)) = self.renaming.take() {
                let name = draft.trim();
                if save && !name.is_empty() && index < self.hosts.len() {
                    self.hosts[index].name = name.to_string();
                    self.save();
                }
            }
        }
        if let Some(message) = failure.or(notice) {
            self.shared.lock().unwrap().message = message;
        }
        if let Some(i) = remove {
            self.hosts.remove(i);
            self.save();
        }
    }

    fn trust_dialog(&mut self, ctx: &egui::Context) {
        let mut decision = None;
        {
            let shared = self.shared.lock().unwrap();
            let Some(candidate) = &shared.to_confirm else { return };
            egui::Window::new("Trust this computer?").collapsible(false).resizable(false).show(ctx, |ui| {
                ui.label(format!("{} at {}", candidate.name, candidate.addr));
                ui.add_space(6.0);
                ui.label("Check that this fingerprint matches the one printed when the agent was installed:");
                ui.monospace(&candidate.fingerprint);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui.button("Trust and add").clicked() {
                        decision = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        decision = Some(false);
                    }
                });
            });
        }
        if let Some(trust) = decision {
            let candidate = self.shared.lock().unwrap().to_confirm.take();
            if let (true, Some(c)) = (trust, candidate) {
                self.hosts.retain(|h| h.addr != c.addr);
                self.hosts.push(Host { name: c.name, addr: c.addr, fingerprint: c.fingerprint, mac: None });
                self.save();
            }
        }
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        self.record_macs();
        ui.heading("Computers");
        ui.add_space(4.0);
        self.computers(ui);
        ui.add_space(12.0);

        ui.horizontal(|ui| {
            let scanning = self.shared.lock().unwrap().scanning;
            if ui.add_enabled(!scanning, egui::Button::new("Scan network")).clicked() {
                start_scan(self.shared.clone());
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.new_addr).hint_text("IP address or name").desired_width(200.0));
            if ui.add_enabled(!self.new_addr.trim().is_empty(), egui::Button::new("Add")).clicked() {
                start_probe(self.shared.clone(), net::with_default_port(&self.new_addr));
                self.new_addr.clear();
            }
        });

        let (found, message): (Vec<(String, String)>, String) = {
            let s = self.shared.lock().unwrap();
            let found = s
                .found
                .iter()
                .filter(|c| !self.hosts.iter().any(|h| h.addr == c.addr))
                .map(|c| (c.name.clone(), c.addr.clone()))
                .collect();
            (found, s.message.clone())
        };
        for (name, addr) in found {
            ui.horizontal(|ui| {
                ui.label(format!("Found {name} at {addr}"));
                if ui.button("Add").clicked() {
                    start_probe(self.shared.clone(), addr.clone());
                }
            });
        }
        if !message.is_empty() {
            ui.add_space(4.0);
            ui.colored_label(egui::Color32::from_rgb(200, 80, 70), message);
        }

        ui.add_space(16.0);
        ui.separator();
        ui.heading("Install on a computer");
        ui.label("Copy rdagent.exe to the computer and run this from an elevated prompt in the same folder:");
        let mut command = install_command(&self.public_key);
        // A read-only text box, so the command can also be selected and copied by hand.
        ui.add(
            egui::TextEdit::multiline(&mut command)
                .font(egui::TextStyle::Monospace)
                .desired_width(f32::INFINITY)
                .desired_rows(2),
        );
        ui.horizontal(|ui| {
            if ui.button("Copy command").clicked() {
                self.copied = Some(if rd_common::clipboard::set_text(&install_command(&self.public_key)) {
                    (Instant::now(), true)
                } else {
                    (Instant::now(), false)
                });
            }
            match self.copied {
                Some((at, true)) if at.elapsed() < Duration::from_secs(3) => {
                    ui.label("Copied to the clipboard.");
                }
                Some((at, false)) if at.elapsed() < Duration::from_secs(3) => {
                    ui.colored_label(egui::Color32::from_rgb(200, 80, 70), "The clipboard is busy. Try again.");
                }
                _ => {}
            }
        });
    }
}

impl eframe::App for Dashboard {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| self.draw(ui));
        });
        self.trust_dialog(&ctx);
        // Picks up status and scan results from the background threads.
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}
