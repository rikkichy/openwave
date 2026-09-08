"""Non-blocking confirmation and progress for this installation's removal."""

from gi.repository import Adw, GLib, Gtk

from . import uninstall


class UninstallDialog(Adw.Window):
    def __init__(self, application):
        super().__init__(application=application, title="Uninstall OpenWave",
                         modal=True, default_width=620, default_height=520)
        self._app = application
        self._plan = None
        self._closed = False
        self._busy = False
        self._delete_settings = False
        parent = application._window or application._setup_window
        if parent is not None:
            self.set_transient_for(parent)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12,
                      margin_top=18, margin_bottom=18, margin_start=18, margin_end=18)
        self.set_content(box)
        self._heading = Gtk.Label(label="Inspecting installation…", xalign=0)
        self._heading.add_css_class("title-2")
        box.append(self._heading)
        scroll = Gtk.ScrolledWindow(vexpand=True,
                                   hscrollbar_policy=Gtk.PolicyType.NEVER)
        self._details = Gtk.Label(xalign=0, yalign=0, wrap=True, selectable=True)
        scroll.set_child(self._details)
        box.append(scroll)
        self._settings = Gtk.CheckButton(label="Delete settings and saved scenes", active=False)
        box.append(self._settings)
        self._status = Gtk.Label(xalign=0, wrap=True)
        box.append(self._status)
        buttons = Gtk.Box(spacing=12, halign=Gtk.Align.END)
        self._cancel = Gtk.Button(label="Cancel")
        self._cancel.connect("clicked", lambda *_: self.close())
        buttons.append(self._cancel)
        self._accept = Gtk.Button(label="Uninstall", sensitive=False)
        self._accept.add_css_class("destructive-action")
        self._accept.connect("clicked", self._execute)
        buttons.append(self._accept)
        box.append(buttons)
        self.set_default_widget(self._cancel)
        self._cancel.grab_focus()
        self.connect("close-request", self._on_close)
        application.uninstall_async(self._inspect, self._inspected, self._inspection_failed)

    @staticmethod
    def _inspect():
        plan = uninstall.inspect()
        return plan, uninstall.describe(plan)

    def _inspected(self, inspected):
        if self._closed or self._app._uninstall_pending:
            return
        self._plan, description = inspected
        self._heading.set_label("Uninstall OpenWave?" if self._plan.remove_application
                                else "Prepare OpenWave removal?")
        self._details.set_label(description)
        self._accept.set_label("Uninstall" if self._plan.remove_application else "Prepare removal")
        self._accept.set_sensitive(self._plan.can_execute)
        self._settings.set_sensitive(self._plan.can_execute)
        self._status.set_label("Settings and saved scenes are preserved unless selected above."
                               if self._plan.can_execute else "Removal is blocked. Follow the guidance above.")

    def _inspection_failed(self, error):
        if self._closed:
            return
        self._heading.set_label("Cannot inspect this installation")
        self._details.set_label(str(error))
        self._settings.set_sensitive(False)
        self._cancel.set_label("Close")

    def _execute(self, _button):
        if self._busy or self._plan is None or not self._plan.can_execute:
            return
        if not self._app._uninstall_pending:
            self._delete_settings = self._settings.get_active()
        self._busy = self._app._uninstall_busy = True
        self._accept.set_sensitive(False)
        self._cancel.set_sensitive(False)
        self._settings.set_sensitive(False)
        self._heading.set_label("Stopping OpenWave…")
        self._status.set_label("Waiting for device and audio workers. This window will remain responsive.")
        try:
            self._app.prepare_uninstall()
        except Exception as error:
            self._failed(error)
            return

        def progress(message):
            GLib.idle_add(self._progress, message)

        def execute():
            self._app.drain_uninstall()
            return uninstall.execute(self._plan, delete_settings=self._delete_settings,
                                     stop_running_app=False, progress=progress)

        self._app.uninstall_async(execute, self._completed, self._failed)

    def _progress(self, message):
        if not self._closed and self._busy:
            self._status.set_label(message)
        return False

    def _completed(self, result):
        if not result.success:
            details = result.error or "Some removal steps did not complete."
            if result.removed:
                details += "\n\nAlready removed:\n" + "\n".join(result.removed)
            if result.guidance:
                details += "\n\n" + result.guidance
            self._failed(details)
            return
        self._busy = self._app._uninstall_busy = False
        self._heading.set_label("OpenWave uninstalled" if result.app_removed
                                else "Removal preparation complete")
        details = "\n".join(result.removed)
        if not result.app_removed:
            details += "\n\nThe application has not been removed."
        if result.guidance:
            details += "\n\n" + result.guidance
        self._details.set_label(details.strip())
        self._status.set_label("Settings deletion was requested." if self._delete_settings
                               else "Your settings and saved scenes were preserved.")
        self._accept.set_visible(False)
        self._cancel.set_label("Close OpenWave")
        self._cancel.set_sensitive(True)
        self._cancel.grab_focus()

    def _failed(self, error):
        self._busy = self._app._uninstall_busy = False
        self._heading.set_label("Removal did not complete")
        self._details.set_label(str(error))
        self._status.set_label("Routing may be stopped. Retry the remaining steps, or close OpenWave. "
                               "Restart OpenWave to resume normal operation if it is still installed.")
        self._accept.set_label("Retry")
        self._accept.set_sensitive(True)
        self._cancel.set_label("Close OpenWave")
        self._cancel.set_sensitive(True)
        self._cancel.grab_focus()

    def _on_close(self, _window):
        if self._busy:
            return True
        self._closed = True
        self._app._uninstall_dialog = None
        if self._app._uninstall_pending:
            self._app.quit()
        elif self._app._setup_window is not None:
            self._app.resume_setup()
        return False
