//! System-tray icon and context menu.

use anyhow::Result;
use tray_icon::menu::{Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub struct Tray {
    tray: TrayIcon,
    item_toggle: MenuItem,
    pub id_add: MenuId,
    pub id_open: MenuId,
    pub id_toggle: MenuId,
    pub id_quit: MenuId,
}

impl Tray {
    pub fn build() -> Result<Tray> {
        let add = MenuItem::new("Add rule…", true, None);
        let open = MenuItem::new("Open Cronch", true, None);
        let toggle = MenuItem::new("Pause all", true, None);
        let quit = MenuItem::new("Quit", true, None);

        let menu = Menu::new();
        menu.append(&add)?;
        menu.append(&open)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&toggle)?;
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&quit)?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Cronch")
            .with_icon(crate::icon::make_icon(true))
            .with_menu_on_left_click(false)
            .build()?;

        Ok(Tray {
            id_add: add.id().clone(),
            id_open: open.id().clone(),
            id_toggle: toggle.id().clone(),
            id_quit: quit.id().clone(),
            item_toggle: toggle,
            tray,
        })
    }

    /// Reflect paused state on the tray icon + menu label.
    pub fn set_paused(&self, paused: bool) {
        self.item_toggle
            .set_text(if paused { "Resume all" } else { "Pause all" });
        let _ = self.tray.set_icon(Some(crate::icon::make_icon(!paused)));
    }
}
