/* Text Processing Engine: Zotero 7 bootstrap plugin (no build step).
 *
 * Adds "Send to Text Processing Engine" to the item context menu. The command
 * POSTs the selected regular items (Zotero item JSON plus attachment paths) to
 * http://127.0.0.1:<port>/zotero/import with an X-TPE-Token header.
 * The body format is documented in docs/ZOTERO.md and parsed by the Rust
 * crate tpe-zotero (src/import.rs).
 */
"use strict";

var TPE;

function install() {}

function uninstall() {}

async function startup({ id, version, rootURI }) {
  TPE = {
    id,
    version,
    rootURI,
    SCHEMA: "tpe.zotero.import/1",
    PREF_PORT: "extensions.tpe-zotero.port",
    PREF_TOKEN: "extensions.tpe-zotero.token",
    DEFAULT_PORT: 47821,
    MENU_ID: "tpe-zotero-send",
    FTL: "tpe-zotero.ftl",

    addToWindow(window) {
      const doc = window.document;
      const menu = doc.getElementById("zotero-itemmenu");
      if (!menu || doc.getElementById(this.MENU_ID)) {
        return;
      }
      window.MozXULElement.insertFTLIfNeeded(this.FTL);
      const item = doc.createXULElement("menuitem");
      item.id = this.MENU_ID;
      item.setAttribute("data-l10n-id", "tpe-zotero-send");
      // Fallback label in case the Fluent file is not available.
      item.setAttribute("label", "Send to Text Processing Engine");
      item.addEventListener("command", () => {
        this.sendSelected(window).catch((error) => this.reportError(error));
      });
      menu.appendChild(item);
    },

    removeFromWindow(window) {
      const doc = window.document;
      const item = doc.getElementById(this.MENU_ID);
      if (item) {
        item.remove();
      }
      const ftl = doc.querySelector(`[href="${this.FTL}"]`);
      if (ftl) {
        ftl.remove();
      }
    },

    addToAllWindows() {
      for (const window of Zotero.getMainWindows()) {
        if (window.ZoteroPane) {
          this.addToWindow(window);
        }
      }
    },

    removeFromAllWindows() {
      for (const window of Zotero.getMainWindows()) {
        if (window.ZoteroPane) {
          this.removeFromWindow(window);
        }
      }
    },

    // Metadata and attachment paths of the selected regular items.
    async collect(items) {
      const out = [];
      for (const item of items) {
        if (!item.isRegularItem()) {
          continue;
        }
        const attachments = [];
        for (const attachmentID of item.getAttachments()) {
          const attachment = Zotero.Items.get(attachmentID);
          if (!attachment) {
            continue;
          }
          const linkMode = attachment.attachmentLinkMode;
          let path = null;
          if (linkMode !== Zotero.Attachments.LINK_MODE_LINKED_URL) {
            // Resolves to false when the file is missing on this computer.
            path = (await attachment.getFilePathAsync()) || null;
          }
          attachments.push({
            key: attachment.key,
            title: attachment.getField("title") || null,
            content_type: attachment.attachmentContentType || null,
            link_mode: linkMode,
            path,
            url: attachment.getField("url") || null,
          });
        }
        out.push({
          key: item.key,
          library_id: item.libraryID,
          data: item.toJSON(),
          attachments,
        });
      }
      return out;
    },

    async sendSelected(window) {
      const selected = window.ZoteroPane.getSelectedItems();
      const items = await this.collect(selected);
      if (items.length === 0) {
        this.notify("Nothing to send", "Select one or more regular items (not notes or attachments).");
        return;
      }
      const token = String(Zotero.Prefs.get(this.PREF_TOKEN, true) || "");
      if (!token) {
        this.notify(
          "Token not set",
          `Set ${this.PREF_TOKEN} in Settings > Advanced > Config Editor to the token shown by the app.`
        );
        return;
      }
      const port = Number(Zotero.Prefs.get(this.PREF_PORT, true)) || this.DEFAULT_PORT;
      const body = {
        schema: this.SCHEMA,
        plugin_version: this.version,
        zotero_version: Zotero.version,
        items,
      };
      await Zotero.HTTP.request("POST", `http://127.0.0.1:${port}/zotero/import`, {
        body: JSON.stringify(body),
        headers: {
          "Content-Type": "application/json",
          "X-TPE-Token": token,
        },
        successCodes: [200, 201, 202, 204],
        timeout: 30000,
      });
      this.notify("Sent to Text Processing Engine", `${items.length} item(s) sent.`);
    },

    reportError(error) {
      // Never log the request headers: they contain the token.
      const status = error && error.status;
      const text = status
        ? `The app answered HTTP ${status}.`
        : "Could not reach the app. Is it running with the local server enabled?";
      this.notify("Send failed", text);
      Zotero.logError(error);
    },

    notify(title, text) {
      const progress = new Zotero.ProgressWindow({ closeOnClick: true });
      progress.changeHeadline(title);
      progress.addDescription(text);
      progress.show();
      progress.startCloseTimer(5000);
    },
  };
  TPE.addToAllWindows();
}

function onMainWindowLoad({ window }) {
  if (TPE) {
    TPE.addToWindow(window);
  }
}

function onMainWindowUnload({ window }) {
  if (TPE) {
    TPE.removeFromWindow(window);
  }
}

function shutdown() {
  if (TPE) {
    TPE.removeFromAllWindows();
  }
  TPE = undefined;
}
