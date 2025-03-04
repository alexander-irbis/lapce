pub mod plugin_install_status;

use std::{collections::HashSet, sync::Arc};

use anyhow::Result;
use druid::{ExtEventSink, Target, WidgetId};
use indexmap::IndexMap;
use lapce_proxy::plugin::{download_volt, wasi::find_all_volts};
use lapce_rpc::plugin::{VoltInfo, VoltMetadata};
use parking_lot::Mutex;
use plugin_install_status::PluginInstallStatus;
use serde::{Deserialize, Serialize};
use strum_macros::Display;

use crate::{
    command::{LapceUICommand, LAPCE_UI_COMMAND},
    config::LapceConfig,
    markdown::parse_markdown,
    proxy::LapceProxy,
};

#[derive(Clone)]
pub struct VoltsList {
    pub total: usize,
    pub volts: IndexMap<String, VoltInfo>,
    pub status: PluginLoadStatus,
    pub loading: Arc<Mutex<bool>>,
    pub event_sink: ExtEventSink,
    pub query: String,
}

impl VoltsList {
    pub fn new(event_sink: ExtEventSink) -> Self {
        Self {
            volts: IndexMap::new(),
            total: 0,
            status: PluginLoadStatus::Loading,
            loading: Arc::new(Mutex::new(false)),
            query: "".to_string(),
            event_sink,
        }
    }

    pub fn update_query(&mut self, query: String) {
        if self.query == query {
            return;
        }

        self.query = query;
        self.volts.clear();
        self.total = 0;
        self.status = PluginLoadStatus::Loading;

        let event_sink = self.event_sink.clone();
        let query = self.query.clone();
        std::thread::spawn(move || match PluginData::load_volts(&query, 0) {
            Ok(info) => {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::LoadPlugins(info),
                    Target::Auto,
                );
            }
            Err(_) => {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::LoadPluginsFailed,
                    Target::Auto,
                );
            }
        });
    }

    pub fn load_more(&self) {
        if self.all_loaded() {
            return;
        }

        let mut loading = self.loading.lock();
        if *loading {
            return;
        }
        *loading = true;

        let offset = self.volts.len();
        let local_loading = self.loading.clone();
        let event_sink = self.event_sink.clone();
        let query = self.query.clone();
        std::thread::spawn(move || match PluginData::load_volts(&query, offset) {
            Ok(info) => {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::LoadPlugins(info),
                    Target::Auto,
                );
            }
            Err(_) => {
                *local_loading.lock() = false;
            }
        });
    }

    fn all_loaded(&self) -> bool {
        self.volts.len() == self.total
    }

    pub fn update_volts(&mut self, info: &PluginsInfo) {
        self.total = info.total;
        for v in &info.plugins {
            self.volts.insert(v.id(), v.clone());
        }
        self.status = PluginLoadStatus::Success;
        *self.loading.lock() = false;
    }

    pub fn failed(&mut self) {
        self.status = PluginLoadStatus::Failed;
    }
}

#[derive(Clone)]
pub struct PluginData {
    pub widget_id: WidgetId,
    pub search_editor: WidgetId,
    pub installed_id: WidgetId,
    pub uninstalled_id: WidgetId,

    pub volts: VoltsList,
    pub installing: IndexMap<String, PluginInstallStatus>,
    pub installed: IndexMap<String, VoltMetadata>,
    pub disabled: HashSet<String>,
    pub workspace_disabled: HashSet<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub enum PluginLoadStatus {
    Loading,
    Failed,
    Success,
}

#[derive(Deserialize, Serialize)]
pub struct PluginsInfo {
    pub plugins: Vec<VoltInfo>,
    pub total: usize,
}

impl PluginData {
    pub fn new(
        tab_id: WidgetId,
        disabled: Vec<String>,
        workspace_disabled: Vec<String>,
        event_sink: ExtEventSink,
    ) -> Self {
        {
            let event_sink = event_sink.clone();
            std::thread::spawn(move || {
                Self::load(tab_id, event_sink);
            });
        }

        Self {
            widget_id: WidgetId::next(),
            search_editor: WidgetId::next(),
            installed_id: WidgetId::next(),
            uninstalled_id: WidgetId::next(),
            volts: VoltsList::new(event_sink),
            installing: IndexMap::new(),
            installed: IndexMap::new(),
            disabled: HashSet::from_iter(disabled.into_iter()),
            workspace_disabled: HashSet::from_iter(workspace_disabled.into_iter()),
        }
    }

    fn load(tab_id: WidgetId, event_sink: ExtEventSink) {
        for meta in find_all_volts() {
            if meta.wasm.is_none() {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::VoltInstalled(meta, false),
                    Target::Widget(tab_id),
                );
            }
        }

        match Self::load_volts("", 0) {
            Ok(info) => {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::LoadPlugins(info),
                    Target::Widget(tab_id),
                );
            }
            Err(_) => {
                let _ = event_sink.submit_command(
                    LAPCE_UI_COMMAND,
                    LapceUICommand::LoadPluginsFailed,
                    Target::Widget(tab_id),
                );
            }
        }
    }

    pub fn plugin_disabled(&self, id: &str) -> bool {
        self.disabled.contains(id) || self.workspace_disabled.contains(id)
    }

    pub fn plugin_status(&self, id: &str) -> PluginStatus {
        if self.plugin_disabled(id) {
            return PluginStatus::Disabled;
        }

        if let Some(meta) = self.installed.get(id) {
            if let Some(volt) = self.volts.volts.get(id) {
                if meta.version == volt.version {
                    PluginStatus::Installed
                } else {
                    PluginStatus::Upgrade
                }
            } else {
                PluginStatus::Installed
            }
        } else {
            PluginStatus::Install
        }
    }

    fn load_volts(query: &str, offset: usize) -> Result<PluginsInfo> {
        let url = format!(
            "https://plugins.lapce.dev/api/v1/plugins?q={query}&offset={offset}"
        );
        let plugins: PluginsInfo = reqwest::blocking::get(&url)?.json()?;
        Ok(plugins)
    }

    pub fn download_readme(
        widget_id: WidgetId,
        volt: &VoltInfo,
        config: &LapceConfig,
        event_sink: ExtEventSink,
    ) -> Result<()> {
        let url = format!(
            "https://plugins.lapce.dev/api/v1/plugins/{}/{}/{}/readme",
            volt.author, volt.name, volt.version
        );
        let resp = reqwest::blocking::get(url)?;
        if resp.status() != 200 {
            let text = parse_markdown("Plugin doesn't have a README", 2.0, config);
            let _ = event_sink.submit_command(
                LAPCE_UI_COMMAND,
                LapceUICommand::UpdateVoltReadme(text),
                Target::Widget(widget_id),
            );
            return Ok(());
        }
        let text = resp.text()?;
        let text = parse_markdown(&text, 2.0, config);
        let _ = event_sink.submit_command(
            LAPCE_UI_COMMAND,
            LapceUICommand::UpdateVoltReadme(text),
            Target::Widget(widget_id),
        );
        Ok(())
    }

    pub fn install_volt(proxy: Arc<LapceProxy>, volt: VoltInfo) -> Result<()> {
        proxy.core_rpc.volt_installing(volt.clone(), "".to_string());

        if volt.wasm {
            proxy.proxy_rpc.install_volt(volt);
        } else {
            std::thread::spawn(move || -> Result<()> {
                let download_volt_result = download_volt(&volt);
                if let Err(err) = download_volt_result {
                    log::warn!("download_volt err: {err:?}");
                    proxy.core_rpc.volt_installing(
                        volt.clone(),
                        "Could not download Plugin".to_string(),
                    );
                    return Ok(());
                }

                let meta = download_volt_result?;
                proxy.core_rpc.volt_installed(meta, false);
                Ok(())
            });
        }
        Ok(())
    }

    pub fn remove_volt(proxy: Arc<LapceProxy>, meta: VoltMetadata) -> Result<()> {
        proxy.core_rpc.volt_removing(meta.clone(), "".to_string());
        if meta.wasm.is_some() {
            proxy.proxy_rpc.remove_volt(meta);
        } else {
            std::thread::spawn(move || -> Result<()> {
                let path = meta.dir.as_ref().ok_or_else(|| {
                    proxy.core_rpc.volt_removing(
                        meta.clone(),
                        "Plugin Directory does not exist".to_string(),
                    );
                    anyhow::anyhow!("don't have dir")
                })?;
                if std::fs::remove_dir_all(path).is_err() {
                    proxy.core_rpc.volt_removing(
                        meta.clone(),
                        "Could not remove Plugin Directory".to_string(),
                    );
                } else {
                    proxy.core_rpc.volt_removed(meta.info(), false);
                }
                Ok(())
            });
        }
        Ok(())
    }
}

#[derive(Display, PartialEq, Eq, Clone)]
pub enum PluginStatus {
    Installed,
    Install,
    Upgrade,
    Disabled,
}
