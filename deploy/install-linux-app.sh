#!/usr/bin/env bash
# Installs the desktop app for the current user: binary, icon, launcher entry, and on KDE
# Plasma and GNOME a Ctrl+Alt+C shortcut and a pinned dock icon. Safe to re-run; it also upgrades.
set -euo pipefail
cd "$(dirname "$0")/../app"

[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

npm install --no-audit --no-fund
npx tauri build --no-bundle

data="${XDG_DATA_HOME:-$HOME/.local/share}"
bin="$HOME/.local/bin/rustyclaude"
desktop="dev.rustyclaude.app.desktop"   # must match the Wayland app id (tauri.conf identifier)

mkdir -p "$(dirname "$bin")" "$data/applications" "$data/icons/hicolor/256x256/apps" "$data/kglobalaccel"
# Replace rather than overwrite, so this works while the app is running.
install -m755 ../target/release/rustyclaude-app "$bin.new" && mv -f "$bin.new" "$bin"
install -m644 src-tauri/icons/128x128@2x.png "$data/icons/hicolor/256x256/apps/rustyclaude.png"

# Earlier versions used RustyClaude.desktop.
old=RustyClaude.desktop
rm -f "$data/applications/$old" "$data/kglobalaccel/$old"
rc="${XDG_CONFIG_HOME:-$HOME/.config}/plasma-org.kde.plasma.desktop-appletsrc"
[ -e "$rc" ] && sed -i "s|applications:$old|applications:$desktop|g" "$rc"
if command -v gsettings >/dev/null && gsettings list-schemas 2>/dev/null | grep -qx org.gnome.shell; then
  gsettings set org.gnome.shell favorite-apps "$(gsettings get org.gnome.shell favorite-apps | sed "s/'$old'/'$desktop'/")"
fi

cat > "$data/applications/$desktop" <<EOF
[Desktop Entry]
Type=Application
Name=RustyClaude
Comment=Several Claude accounts in one window
Exec=$bin
Icon=$data/icons/hicolor/256x256/apps/rustyclaude.png
Terminal=false
Categories=Network;Development;
StartupWMClass=dev.rustyclaude.app
StartupNotify=false
SingleMainWindow=true
Actions=Toggle;

[Desktop Action Toggle]
Name=Show or Hide
Exec=$bin --toggle
X-KDE-Shortcuts=Ctrl+Alt+C
EOF
# Global key grabs don't work on Wayland, so KDE owns the shortcut.
ln -sf "$data/applications/$desktop" "$data/kglobalaccel/$desktop"
update-desktop-database "$data/applications" 2>/dev/null || true
kbuildsycoca6 2>/dev/null || true
gtk-update-icon-cache -q -t -f "$data/icons/hicolor" 2>/dev/null || true

# KDE Plasma dock. If Plasma isn't running, edit its config for the next login instead.
rc="${XDG_CONFIG_HOME:-$HOME/.config}/plasma-org.kde.plasma.desktop-appletsrc"
qdbus=$(command -v qdbus6 || command -v qdbus-qt6 || command -v qdbus || true)
if [ -e "$rc" ] && ! grep -q "applications:$desktop" "$rc"; then
  [ -e "$rc.pre-rustyclaude" ] || cp "$rc" "$rc.pre-rustyclaude"
  if pgrep -x plasmashell >/dev/null && [ -n "$qdbus" ]; then
    "$qdbus" org.kde.plasmashell /PlasmaShell org.kde.PlasmaShell.evaluateScript "
      const entry = 'applications:$desktop';
      for (const p of panels()) for (const w of p.widgets()) {
        if (w.type !== 'org.kde.plasma.icontasks') continue;
        w.currentConfigGroup = ['General'];
        const list = String(w.readConfig('launchers', '')).split(',').filter(Boolean);
        if (list.includes(entry)) continue;
        let i = list.findIndex(l => l.includes('systemsettings'));
        if (i < 0) i = list.length;
        list.splice(i, 0, entry);
        w.writeConfig('launchers', list.join(','));
        w.reloadConfig();
      }"
  else
    sed -i "/^launchers=/ s|\$|,applications:$desktop|" "$rc"
  fi
fi
if pgrep -x kglobalaccel5 >/dev/null || pgrep -x kglobalacceld >/dev/null; then
  systemctl --user restart "plasma-kglobalaccel*" 2>/dev/null || true
fi

# GNOME dock and shortcut.
if gsettings list-schemas 2>/dev/null | grep -qx org.gnome.shell; then
  favs=$(gsettings get org.gnome.shell favorite-apps)
  case "$favs" in
    *"'$desktop'"*) ;;
    "@as []") gsettings set org.gnome.shell favorite-apps "['$desktop']" ;;
    *) gsettings set org.gnome.shell favorite-apps "${favs%]}, '$desktop']" ;;
  esac
  media=org.gnome.settings-daemon.plugins.media-keys
  path=/org/gnome/settings-daemon/plugins/media-keys/custom-keybindings/rustyclaude/
  keys=$(gsettings get $media custom-keybindings)
  case "$keys" in
    *"$path"*) ;;
    "@as []") gsettings set $media custom-keybindings "['$path']" ;;
    *) gsettings set $media custom-keybindings "${keys%]}, '$path']" ;;
  esac
  kb="$media.custom-keybinding:$path"
  gsettings set "$kb" name "RustyClaude"
  gsettings set "$kb" command "$bin --toggle"
  gsettings set "$kb" binding "<Primary><Alt>c"
fi

echo "Installed $bin"
