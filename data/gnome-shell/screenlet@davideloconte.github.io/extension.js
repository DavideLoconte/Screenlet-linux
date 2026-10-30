// Quick Settings toggle for Screenlet: all the work happens in the Screenlet
// application; this only talks to it over D-Bus (starting it on demand).
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const BUS_NAME = 'io.github.davideloconte.Screenlet';
const OBJECT_PATH = '/io/github/davideloconte/Screenlet';
const ICON = 'io.github.davideloconte.Screenlet-symbolic';
const ScreenletProxy = Gio.DBusProxy.makeProxyWrapper(`
<node>
  <interface name="io.github.davideloconte.Screenlet1">
    <method name="Connect"/>
    <method name="Disconnect"/>
    <method name="ShowWindow"/>
    <method name="Quit"/>
    <property name="Active" type="b" access="read"/>
    <property name="Title" type="s" access="read"/>
  </interface>
</node>`);

const ITALIAN = {Off: 'Spento', Settings: 'Impostazioni', Quit: 'Esci', Tablet: 'Tablet'};
const italian = GLib.get_language_names()[0].startsWith('it');
const _ = text => (italian && ITALIAN[text]) || text;

const ScreenletToggle = GObject.registerClass(
class ScreenletToggle extends QuickMenuToggle {
    _init(proxy) {
        super._init({title: _('Tablet'), iconName: ICON, toggleMode: false});
        this._proxy = proxy;
        this.connect('clicked', () => {
            (proxy.Active ? proxy.DisconnectAsync() : proxy.ConnectAsync()).catch(logError);
        });
        this.menu.setHeader(ICON, 'Screenlet');
        this.menu.addAction(_('Settings'), () => {
            proxy.ShowWindowAsync().catch(logError);
            Main.panel.closeQuickSettings();
        });
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this.menu.addAction(_('Quit'), () => proxy.QuitAsync().catch(logError));
    }

    sync() {
        const running = this._proxy.g_name_owner !== null;
        this.checked = running && Boolean(this._proxy.Active);
        this.subtitle = running ? this._proxy.Title : _('Off');
        this.menu.setHeader(ICON, 'Screenlet', running ? this._proxy.Title : null);
    }
});

const ScreenletIndicator = GObject.registerClass(
class ScreenletIndicator extends SystemIndicator {
    _init() {
        super._init();
        this._icon = this._addIndicator();
        this._icon.iconName = ICON;
        this._icon.visible = false;
        this._cancellable = new Gio.Cancellable();
        // Screenlet is not started because the shell came up: only a click starts it.
        new ScreenletProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, (proxy, error) => {
            if (error) {
                if (!error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    logError(error, 'Screenlet');
                return;
            }
            this._proxy = proxy;
            this._toggle = new ScreenletToggle(proxy);
            this.quickSettingsItems.push(this._toggle);
            this._signals = [
                proxy.connect('g-properties-changed', () => this._sync()),
                proxy.connect('notify::g-name-owner', () => this._sync()),
            ];
            this._sync();
            Main.panel.statusArea.quickSettings.addExternalIndicator(this);
        }, this._cancellable, Gio.DBusProxyFlags.DO_NOT_AUTO_START_AT_CONSTRUCTION);
    }

    _sync() {
        this._toggle.sync();
        this._icon.visible = this._proxy.g_name_owner !== null && Boolean(this._proxy.Active);
    }

    destroy() {
        this._cancellable.cancel();
        this._signals?.forEach(id => this._proxy.disconnect(id));
        this.quickSettingsItems.forEach(item => item.destroy());
        super.destroy();
    }
});

export default class ScreenletExtension extends Extension {
    enable() {
        this._indicator = new ScreenletIndicator();
    }

    disable() {
        this._indicator.destroy();
        this._indicator = null;
    }
}
