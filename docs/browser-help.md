# The browser and BlazingDL aren't connected

[← Back to docs index](README.md)

When the browser can't hand a download to BlazingDL, it keeps the download itself, so nothing is lost. The extension's toolbar button then shows a red **!**. Click it to see what's wrong, then **How to fix it** brings you to the matching section below.

To check BlazingDL's side, open **Options → General** in BlazingDL and click **Test browser integration...**. It checks that browser integration is on, that this is the installed copy, that the browsers can find BlazingDL, and which browsers' extension has connected; **How to fix it** opens the section for the first that fails.

<a id="not-installed"></a>
## BlazingDL isn't installed, or browser integration isn't set up {#not-installed}

The browser can't find the small helper program it talks to. It is registered when BlazingDL is installed.

1. Install BlazingDL with the setup from [GitHub Releases](https://github.com/Khuraie/BlazingDL/releases/latest), and keep **Set up browser integration** ticked.
2. Already installed? Open BlazingDL, then **Options → General → Test browser integration**, and click **Set it up** if it offers to.
3. The portable copy can't be connected: it writes nothing outside its own folder. Use the installed copy for browser integration. See [Portable mode](portable.md).
4. **Started BlazingDL from inside a zip?** WinRAR, 7-Zip and Windows itself run a program from inside a zip by unpacking it to a temporary folder, which they delete afterwards. A copy run that way can't be connected (**Test browser integration** says **This copy runs from a temporary folder that will be deleted**): unzip it to a folder of its own first. Start your installed BlazingDL once to re-register browser integration.
5. On Linux and macOS, BlazingDL sets browser integration up itself each time it starts: start BlazingDL once. **Test browser integration** says what went wrong if it couldn't, such as **The helper program the browsers start is missing**: **Set it up** puts it back.
6. Restart the browser after setting it up.
7. **Zen, Helium, Brave Origin or another browser built on Firefox or Chromium, as your only browser on Linux or macOS?** Update BlazingDL and start it once so it registers browser integration for the browser.
8. **The AppImage on Linux:** the AppImage's files are gone once it quits, so BlazingDL keeps a copy of its helper program in its data folder (`~/.local/share/blazingdl/browser-host`) and points the browsers there. Start BlazingDL once after moving or renaming the AppImage, or after an AppImage manager (Gear Lever, AppImageLauncher and the like) moved it, so the helper knows where to start it from.
9. **Zen on Windows:** Zen 1.21.15b and later have a bug of their own that keeps add-ons from reaching their helper program ([zen-browser/desktop#15432](https://github.com/zen-browser/desktop/issues/15432)), with any add-on that uses one. Until Zen fixes it, Zen 1.21.14b works.

<a id="not-running"></a>
## BlazingDL didn't start in time {#not-running}

When BlazingDL isn't running, the browser starts it for each download and waits for it. It waited about 20 seconds this time and gave up.

- Start BlazingDL yourself and try the download again.
- A first start, a slow disk, or an antivirus scanning BlazingDL can take that long. Next time it is usually quick.
- With a VPN or proxy program, allow connections to `127.0.0.1` (your own computer): the browser's helper reaches BlazingDL through one.
- If BlazingDL never opens, see [The setup or BlazingDL won't start](troubleshooting.md).

<a id="host-failed"></a>
## The connection to BlazingDL failed {#host-failed}

The browser started BlazingDL's helper, but it stopped before answering. Security software is the usual cause.

- Allow `blazingdl-nmhost.exe` (in BlazingDL's folder) in your antivirus or anti-malware program.
- Reinstall BlazingDL to restore its files.
- On Linux, with Firefox or Chromium installed as a Snap, see [Flatpak and Snap browsers](#linux-sandbox).
- Uninstalled BlazingDL on Linux? The `.deb` and `.rpm` take their browser files with them; the AppImage's and the `.tar.gz`'s stay in your home folder: [remove them](getting-started.md#uninstall-linux), and the extension too.

<a id="forbidden"></a>
## BlazingDL doesn't accept this copy of the extension {#forbidden}

BlazingDL's helper serves only its own extension. The browser says this copy isn't one it knows.

- **Two copies installed?** Open the browser's extensions page (`about:addons`, `chrome://extensions`, `edge://extensions`, `brave://extensions`) and keep only one **BlazingDL** extension.
- **A copy from elsewhere?** Remove it and install the Firefox add-on from [addons.mozilla.org](https://addons.mozilla.org/firefox/addon/blazingdl/), or load the Chromium extension unpacked from the release zip (see [Browser integration](browser-integration.md)).
- **An older BlazingDL?** Update BlazingDL from [GitHub Releases](https://github.com/Khuraie/BlazingDL/releases/latest): a newer extension may have an ID an old helper doesn't know.

<a id="update-app"></a>
## The extension is newer than BlazingDL {#update-app}

Downloads still work, but some of the extension's features need a newer BlazingDL. Download the latest version from [GitHub Releases](https://github.com/Khuraie/BlazingDL/releases/latest).

If you're already on the latest release, there's nothing for you to do. The extension updates by itself through its store and can get ahead of the app for a while. Everything keeps working, and the message goes away with the next version of BlazingDL.

<a id="too-large"></a>
## A download's details were too large {#too-large}

A download carries its site's cookies (and a form's data) to BlazingDL. A few sites set so many that together they exceed what BlazingDL accepts from the browser (1 MB). The browser kept this one. Signing out and back in to the site usually trims them.

**Download all links** and **Download selected links** send a long list of links in parts of up to 1 MB, so a page with tens of thousands of links reaches Link grabber whole. A single link longer than that, which no server takes, can't be sent: it is left out, the others still reach Link grabber, and this page opens to say so. When a site's cookies alone fill the 1 MB, no link can go with them: sign out and back in to the site, as above.

<a id="options"></a>
## Downloads stay with the browser although BlazingDL is connected {#options}

The extension is connected, but BlazingDL's own settings leave this download to the browser. Check, in BlazingDL:

1. **Options → General → Use advanced browser integration** is on, and your browser is ticked under **Capture downloads from the following browsers**.
2. The file's type is listed in **Options → File types**.
3. The site isn't in **Don't start downloading automatically from the following sites**, nor the address in **Don't start downloading automatically from the following addresses**.
4. The file isn't smaller than **Don't take files smaller than** in **Options → File types**.
5. For a `.torrent` file: **Open magnet links and .torrent files with BlazingDL** is on in **Tasks → Torrent settings**.

Also check the extension's toolbar button: with **Take downloads from this browser** off, everything stays with the browser.

<a id="opened"></a>
## The browser opens PDFs and music instead of sending them {#opened}

BlazingDL also takes the file types the browser would show or play itself (a PDF, an MP3, also in a frame of a page), when they're listed in **Options → File types** and **Also take files the browser would open** is ticked in **Options → General → Keys...**. This needs a Chromium browser (Chrome, Edge, Brave, Opera, Vivaldi) version 128 or later, or Firefox. Sites in BlazingDL's excluded sites keep opening them in the browser. Hold **Alt** (**Option** on a Mac) while clicking a link to leave it to the browser: the extension follows it as a plain click does (the browser opens or downloads it, and a site's bot check runs in the tab first), instead of the browser's own Alt+click, which saves the link without opening it in Chromium browsers and Safari.

<a id="store-youtube"></a>
## No Download video button on YouTube in Chrome, Edge or Brave {#store-youtube}

You can always save a video without the button: in BlazingDL, open **Tasks → Download video from a page** and paste the video's address.

<a id="site-access"></a>
## The extension can't read this site {#site-access}

In Chrome, Edge and Brave you can limit an extension to some sites (right-click its toolbar button → **This can read and change site data**). On a site it may not read, the extension can't pass your sign-in on, so it leaves the download to the browser. Choose **On all sites** to let BlazingDL take downloads everywhere.

<a id="loaded-by-hand"></a>
## The popup says this copy was loaded by hand {#loaded-by-hand}

A copy loaded with **Load unpacked** (Chrome, Edge, Brave, Opera, Vivaldi) or **Load Temporary Add-on** (Firefox) doesn't update itself, and its toolbar button's popup says so, with a link to the browser's store. In Firefox, install the add-on from [addons.mozilla.org](https://addons.mozilla.org/firefox/addon/blazingdl/) and remove the temporary copy on `about:addons` so the browser doesn't run two.

<a id="firefox-temporary"></a>
## Firefox forgets the extension after a restart {#firefox-temporary}

An add-on loaded with **Load Temporary Add-on** (from `about:debugging`) is removed when Firefox closes. Remove it and install the add-on from [its page on addons.mozilla.org](https://addons.mozilla.org/firefox/addon/blazingdl/) instead: Firefox keeps that one, and updates it.

<a id="linux-sandbox"></a>
## Linux: Flatpak and Snap browsers {#linux-sandbox}

Browsers installed as a Flatpak or a Snap run in a sandbox, which decides whether they can reach BlazingDL.

- **Firefox as a Snap** (Ubuntu's own Firefox) reaches BlazingDL through the desktop, which reads the browser files in your home folder and those the `.deb` or `.rpm` installs for everyone. The first time the extension calls BlazingDL, the desktop asks **Allow Firefox to start WebExtension backend?** for BlazingDL: choose **Allow**. After **Don't allow**, the extension says the connection failed and the desktop doesn't ask again; to be asked again, run this in a terminal, then use the extension again:

  `gdbus call --session --dest org.freedesktop.impl.portal.PermissionStore --object-path /org/freedesktop/impl/portal/PermissionStore --method org.freedesktop.impl.portal.PermissionStore.Delete webextensions com.blazingdl.host`
- **Chromium as a Snap** (Ubuntu's Chromium) starts BlazingDL's helper inside its sandbox, where it can't reach BlazingDL, so BlazingDL doesn't list it in **Add BlazingDL to your browser**. Use Firefox, or Google Chrome from its own `.deb`.
- **Flatpak browsers** (such as Zen's `app.zen_browser.zen`) can't start BlazingDL's helper, so BlazingDL doesn't list them either. Install the browser from your distribution's packages (or the browser maker's own `.deb`, `.rpm`, tarball or AppImage) to use browser integration.

## Next steps

- [Browser integration →](browser-integration.md)
- [Troubleshooting →](troubleshooting.md)
- [Back to the docs index →](README.md)
