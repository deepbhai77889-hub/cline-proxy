use crate::key_manager::{test_key_connection, ApiKeyEntry, KeyManager};
use crate::logger::LOGGER;
use eframe::egui::{self, Color32, RichText, ScrollArea};
use tokio::sync::mpsc;

#[derive(PartialEq)]
enum Tab {
    Accounts,
    ConsoleLogs,
}

pub struct ProxyApp {
    key_manager: KeyManager,
    current_tab: Tab,

    // Add Key state
    show_add_modal: bool,
    new_key_name: String,
    new_key_val: String,
    test_status: Option<(bool, String)>,
    is_testing: bool,

    // Channels for async test connection
    test_tx: mpsc::UnboundedSender<(String, String)>,
    test_rx: mpsc::UnboundedReceiver<(bool, String)>,

    auto_scroll: bool,
}

impl ProxyApp {
    pub fn new(key_manager: KeyManager) -> Self {
        let (test_tx, mut internal_rx) = mpsc::unbounded_channel::<(String, String)>();
        let (result_tx, test_rx) = mpsc::unbounded_channel::<(bool, String)>();

        // Background worker for testing keys
        tokio::spawn(async move {
            while let Some((name, key)) = internal_rx.recv().await {
                match test_key_connection(&key).await {
                    Ok(msg) => {
                        let _ = result_tx.send((true, format!("Verified for '{name}': {msg}")));
                    }
                    Err(err) => {
                        let _ = result_tx.send((false, format!("Failed for '{name}': {err}")));
                    }
                }
            }
        });

        Self {
            key_manager,
            current_tab: Tab::Accounts,
            show_add_modal: false,
            new_key_name: String::new(),
            new_key_val: String::new(),
            test_status: None,
            is_testing: false,
            test_tx,
            test_rx,
            auto_scroll: true,
        }
    }
}

impl eframe::App for ProxyApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll test results
        if let Ok((success, msg)) = self.test_rx.try_recv() {
            self.is_testing = false;
            self.test_status = Some((success, msg));
        }

        // Top Header
        egui::TopBottomPanel::top("header").show(ctx, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("⚡ Cline Proxy Engine");
                ui.label(RichText::new("localhost:9090").color(Color32::from_rgb(88, 166, 255)).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("● ONLINE").color(Color32::from_rgb(63, 185, 80)).strong());
                });
            });
            ui.add_space(8.0);

            // Tab navigation
            ui.horizontal(|ui| {
                if ui.selectable_label(self.current_tab == Tab::Accounts, "🔑 Accounts (API Keys)").clicked() {
                    self.current_tab = Tab::Accounts;
                }
                if ui.selectable_label(self.current_tab == Tab::ConsoleLogs, "📜 Console Logs").clicked() {
                    self.current_tab = Tab::ConsoleLogs;
                }
            });
            ui.add_space(4.0);
        });

        // Bottom Status Bar
        egui::TopBottomPanel::bottom("footer").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Auth: Bearer public").weak());
                ui.separator();
                ui.label(RichText::new("Failover: Round-Robin Active").weak());
                ui.separator();
                ui.label(RichText::new("Web Dashboard: http://127.0.0.1:9090/dashboard").weak());
            });
        });

        // Main Body
        egui::CentralPanel::default().show(ctx, |ui| {
            match self.current_tab {
                Tab::Accounts => self.render_accounts_tab(ui),
                Tab::ConsoleLogs => self.render_logs_tab(ui),
            }
        });

        // Add Key Modal / Dialog
        if self.show_add_modal {
            self.render_add_key_modal(ctx);
        }

        // Request continuous repaint for smooth live logs
        ctx.request_repaint_after(std::time::Duration::from_millis(300));
    }
}

impl ProxyApp {
    fn render_accounts_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(10.0);
        ui.horizontal(|ui| {
            ui.heading("Configured API Keys");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new("+ Add API Key").strong().color(Color32::WHITE)).clicked() {
                    self.show_add_modal = true;
                    self.new_key_name = String::new();
                    self.new_key_val = String::new();
                    self.test_status = None;
                    self.is_testing = false;
                }
            });
        });
        ui.label(RichText::new("Keys are rotated in Round-Robin order. If any key hits a 429 rate limit or error, requests auto-failover instantly to the next key.").weak());
        ui.add_space(10.0);

        let keys_snapshot: Vec<ApiKeyEntry> = if let Ok(lock) = self.key_manager.keys.try_read() {
            lock.clone()
        } else {
            Vec::new()
        };

        if keys_snapshot.is_empty() {
            ui.vertical_centered(|ui| {
                ui.add_space(40.0);
                ui.label(RichText::new("No API keys added yet.").italics());
                ui.label("Click '+ Add API Key' to add your Cline API keys.");
            });
            return;
        }

        ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("keys_grid")
                .striped(true)
                .min_col_width(80.0)
                .spacing([15.0, 10.0])
                .show(ui, |ui| {
                    ui.label(RichText::new("Name").strong());
                    ui.label(RichText::new("Key").strong());
                    ui.label(RichText::new("Status").strong());
                    ui.label(RichText::new("Total Calls").strong());
                    ui.label(RichText::new("Failures").strong());
                    ui.label(RichText::new("Action").strong());
                    ui.end_row();

                    let mut to_delete = None;
                    let mut to_toggle = None;

                    for k in &keys_snapshot {
                        ui.label(&k.name);

                        // Mask key display
                        let masked = if k.key.len() > 14 {
                            format!("{}...{}", &k.key[..8], &k.key[k.key.len() - 6..])
                        } else {
                            "********".to_string()
                        };
                        ui.label(RichText::new(masked).monospace());

                        // Status badge
                        let (status_text, status_color) = if !k.enabled {
                            ("Disabled", Color32::from_rgb(139, 148, 158))
                        } else if k.failed_calls > 0 {
                            (k.last_status.as_deref().unwrap_or("Error"), Color32::from_rgb(248, 81, 73))
                        } else {
                            (k.last_status.as_deref().unwrap_or("Active"), Color32::from_rgb(63, 185, 80))
                        };
                        ui.label(RichText::new(format!("● {status_text}")).color(status_color));

                        ui.label(k.total_calls.to_string());
                        ui.label(k.failed_calls.to_string());

                        ui.horizontal(|ui| {
                            let toggle_lbl = if k.enabled { "Disable" } else { "Enable" };
                            if ui.button(toggle_lbl).clicked() {
                                to_toggle = Some(k.id.clone());
                            }
                            if ui.button(RichText::new("Delete").color(Color32::from_rgb(248, 81, 73))).clicked() {
                                to_delete = Some(k.id.clone());
                            }
                        });
                        ui.end_row();
                    }

                    if let Some(id) = to_delete {
                        let km = self.key_manager.clone();
                        tokio::spawn(async move {
                            km.remove_key(&id).await;
                        });
                    }
                    if let Some(id) = to_toggle {
                        let km = self.key_manager.clone();
                        tokio::spawn(async move {
                            km.toggle_key(&id).await;
                        });
                    }
                });
        });
    }

    fn render_logs_tab(&mut self, ui: &mut egui::Ui) {
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Live Console Logs");
            ui.checkbox(&mut self.auto_scroll, "Auto-scroll");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear Logs").clicked() {
                    LOGGER.clear();
                }
            });
        });
        ui.add_space(6.0);

        let logs = LOGGER.get_items();

        egui::Frame::canvas(ui.style())
            .fill(Color32::from_rgb(9, 13, 19))
            .stroke(egui::Stroke::new(1.0_f32, Color32::from_rgb(48, 54, 61)))
            .inner_margin(12.0)
            .show(ui, |ui| {
                let scroll = ScrollArea::vertical().stick_to_bottom(self.auto_scroll);
                scroll.show(ui, |ui| {
                    if logs.is_empty() {
                        ui.label(RichText::new("Listening for live requests on localhost:9090...").color(Color32::GRAY).italics());
                    } else {
                        for item in logs {
                            let color = if item.icon.contains("🔴") {
                                Color32::from_rgb(248, 81, 73)
                            } else if item.icon.contains("🟢") || item.icon.contains("📊") {
                                Color32::from_rgb(63, 185, 80)
                            } else if item.icon.contains("🟣") {
                                Color32::from_rgb(210, 168, 255)
                            } else if item.icon.contains("ℹ️") {
                                Color32::from_rgb(88, 166, 255)
                            } else if item.icon.contains("🟤") {
                                Color32::from_rgb(210, 153, 34)
                            } else {
                                Color32::from_rgb(201, 209, 217)
                            };

                            let line = format!("[{}] {} {}", item.timestamp, item.icon, item.text);
                            ui.label(RichText::new(line).color(color).monospace());
                        }
                    }
                });
            });
    }

    fn render_add_key_modal(&mut self, ctx: &egui::Context) {
        egui::Window::new("Add Cline API Key")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .fixed_size([440.0, 260.0])
            .show(ctx, |ui| {
                ui.add_space(6.0);
                ui.label("Account / Key Name:");
                ui.text_edit_singleline(&mut self.new_key_name);

                ui.add_space(6.0);
                ui.label("API Key (sk_... or Bearer workos:...):");
                ui.text_edit_singleline(&mut self.new_key_val);

                ui.add_space(8.0);
                if let Some((success, ref msg)) = self.test_status {
                    let color = if success {
                        Color32::from_rgb(63, 185, 80)
                    } else {
                        Color32::from_rgb(248, 81, 73)
                    };
                    ui.label(RichText::new(msg).color(color));
                } else if self.is_testing {
                    ui.label(RichText::new("⏳ Testing connection with Cline backend...").color(Color32::from_rgb(88, 166, 255)));
                }

                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.show_add_modal = false;
                    }

                    // Test Connection button
                    if ui.add_enabled(!self.is_testing && !self.new_key_val.is_empty(), egui::Button::new("Test Connection")).clicked() {
                        self.is_testing = true;
                        self.test_status = None;
                        let name = if self.new_key_name.is_empty() { "New Key".to_string() } else { self.new_key_name.clone() };
                        let _ = self.test_tx.send((name, self.new_key_val.trim().to_string()));
                    }

                    // Save Key button: ALWAYS runs test first!
                    if ui.add_enabled(!self.is_testing && !self.new_key_val.is_empty(), egui::Button::new(RichText::new("Save Key").strong())).clicked() {
                        let km = self.key_manager.clone();
                        let name = if self.new_key_name.is_empty() { "New Key".to_string() } else { self.new_key_name.clone() };
                        let key = self.new_key_val.trim().to_string();

                        self.is_testing = true;
                        self.test_status = None;

                        let n_save = name.clone();
                        let k_save = key.clone();
                        tokio::spawn(async move {
                            if test_key_connection(&k_save).await.is_ok() {
                                let _ = km.add_key(n_save, k_save).await;
                            }
                        });

                        let _ = self.test_tx.send((name, key));
                        self.show_add_modal = false;
                    }
                });
            });
    }
}
