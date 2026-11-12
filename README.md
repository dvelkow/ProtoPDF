# ProtoPDF

A small, fast PDF reader for Windows and Linux written entirely in Rust. It opens a 200 page manual in under a second, does the page edits people need most often, and prints through the normal Windows print dialog, or through CUPS on Linux.

## What it does

Reading
- Continuous scrolling, zoom (25% to 600%), fit width, fit page
- Page list with thumbnails
- Find text in the whole document, with highlights and next / previous
- Select text with the mouse and copy it
- Links work: links to other pages jump there (Alt+Left goes back), web and mail links open in the browser or mail program
- Contents panel with the document's bookmarks, next to the page thumbnails
- Password protected files
- Fonts that a PDF names but does not embed are taken from Windows (on Linux: the Liberation fonts, which have the same letter widths as Arial, Times New Roman and Courier New), so Cyrillic text shows correctly

Editing
- Rotate a page or all pages
- Delete a page, move a page up or down
- Insert another PDF after the current page or at the end
- Save chosen pages as a new PDF (for example `1-3, 5`)
- Write text on a page, any script, in black, blue or red
- Highlight selected text and stick notes on a page. They are saved as standard PDF comments, so other readers show them too, and comments made elsewhere can be read and removed here
- Fill in forms: text boxes (also the one character per box kind), multi-line boxes, tick boxes, radio buttons and drop-down lists. Tab moves to the next box
- Undo and redo for every edit. Nothing is written to disk until you press Save

Digital signatures
- Every signed PDF is checked when it is opened. A coloured bar says whether the document is unchanged and whether the signer is trusted, and Details lists each signature: who signed, when, the issuer, and any trusted timestamp
- Sign with any certificate in the Windows certificate store, which includes qualified signatures on smart cards and USB tokens. Windows asks for the PIN
- The signature can be shown on the page (click where it should go) or be invisible
- Signing appends to the file, so signatures that are already in it stay valid and several people can sign in turn
- ProtoPDF warns before an edit that would break an existing signature

Printing
- Print button and Ctrl+P open the Windows print dialog: any installed printer, page range, copies
- Landscape pages are turned to fill the sheet, oversized pages are shrunk to the printable area
- Right click a PDF in Explorer and choose Print to print it without opening a window

On Linux
- Signatures are checked with OpenSSL. A signer counts as trusted when the certificate leads to an authority in the system's list (the `ca-certificates` package) or to a root certificate you copy into `~/.local/share/ProtoPDF/trusted` (PEM or DER). Put the root certificate of your national qualified signature provider there to have its signatures shown as trusted
- Signing uses a certificate file (.p12 or .pfx) with its password, chosen in the Sign window
- Printing hands the PDF itself to CUPS, so text prints at the printer's full resolution. ProtoPDF shows its own dialog: printer, pages, copies

## Build

Needs Rust 1.95 or newer. `rust-toolchain.toml` asks rustup for the current stable release, so `cargo` picks it up by itself.

Windows: the MSVC toolchain (Visual Studio Build Tools with the Windows SDK).

Linux (Ubuntu / Debian):

```
sudo apt install build-essential pkg-config libssl-dev libxkbcommon-dev
```

```
cargo build --release
```

The program is `target\release\ProtoPDF.exe` on Windows, a single file with no other dependencies. On Linux it is `target/release/ProtoPDF`. It uses the system's OpenSSL, CUPS (`cups-client`), `xdg-utils` and fonts, which Ubuntu desktops have already.

## Install on Linux and make it the default PDF app

```
./install.sh
```

This copies the program to `~/.local/bin/protopdf`, adds ProtoPDF to the application menu and to "Open With" for PDF files, and makes it the default PDF app. Everything is per user, no root needed. Double clicking a PDF in Files then opens it in ProtoPDF.

`./install.sh --no-default` installs without changing the default. File, "Make ProtoPDF the default PDF app" does it later. `./uninstall.sh` removes everything, and PDF files go back to the system's default app.

## Install on Windows and make it the default PDF app

```
powershell -ExecutionPolicy Bypass -File install.ps1
```

This copies the program to `%LOCALAPPDATA%\Programs\ProtoPDF`, adds a Start menu shortcut and registers ProtoPDF with Windows as a PDF app. No administrator rights are needed, everything is per user.

Windows does not allow a program to appoint itself as the default. That last step is one click by you:

- in ProtoPDF choose File, "Make ProtoPDF the default PDF app", then press "Set default" on the Windows page that opens, or
- right click any PDF, Open with, Choose another app, pick ProtoPDF and press Always

`uninstall.ps1` removes everything again.

## Keys

| Key | Action |
| --- | --- |
| Ctrl+O | Open |
| Ctrl+S, Ctrl+Shift+S | Save, Save as |
| Ctrl+P | Print |
| Ctrl+F, F3, Shift+F3 | Find, next, previous |
| Ctrl+C, Ctrl+A | Copy selection, select all text on the page |
| Ctrl+Z, Ctrl+Y | Undo, redo |
| Ctrl+H | Highlight the selected text |
| Alt+Left | Back to where a link was followed |
| Ctrl+R | Rotate the page right |
| Ctrl + mouse wheel, Ctrl+Plus, Ctrl+Minus | Zoom |
| Ctrl+1, Ctrl+2, Ctrl+0 | Actual size, fit width, fit page |
| Page Up, Page Down, Space, arrows, Home, End | Move through the document |

## Command line

```
ProtoPDF.exe file.pdf                      open a file
ProtoPDF.exe --print file.pdf              print to the default printer, no window
ProtoPDF.exe --print file.pdf --printer "Microsoft Print to PDF" --output out.pdf
ProtoPDF.exe --render file.pdf 1 page.png 1200   render page 1 to a PNG 1200 px wide
ProtoPDF.exe --verify file.pdf             check the signatures and print the result
ProtoPDF.exe --sign in.pdf out.pdf --thumbprint HEX [--page 1 --rect 330,640,520,688 --reason text --location text]
ProtoPDF.exe --register                    register as a PDF app and open the Windows default apps page
ProtoPDF.exe --unregister                  remove the registration
```

On Linux the switches are the same, with these differences:

```
protopdf --register [--quiet]              add the menu entry and, without --quiet, become the default PDF app
protopdf --print file.pdf --output out.pdf write the PDF that would go to the printer to a file instead
PROTOPDF_P12_PASSWORD=... protopdf --sign in.pdf out.pdf --p12 me.p12 [--page 1 --rect ... --reason ... --location ...]
```

## How it is built

| Part | Crate |
| --- | --- |
| Rendering and text extraction | [hayro](https://github.com/LaurenzV/hayro), a pure Rust PDF renderer |
| Page edits and saving | [lopdf](https://github.com/J-F-Liu/lopdf) |
| Fonts for added text | [ttf-parser](https://github.com/harfbuzz/ttf-parser) and [subsetter](https://github.com/typst/subsetter) |
| Window and controls | [egui / eframe](https://github.com/emilk/egui) |
| Signatures | Windows CryptoAPI through the windows crate; on Linux [OpenSSL](https://github.com/sfackler/rust-openssl): PKCS#7 / CMS signing, verification, certificate chains, timestamps |
| Print dialog, printing, registry | [windows](https://github.com/microsoft/windows-rs), [winreg](https://github.com/gentoo90/winreg-rs); on Linux CUPS (`lp`), and a freedesktop.org desktop entry with `xdg-mime` |

Source layout: `src/render.rs` (background render threads), `src/text.rs` (search and selection), `src/edit.rs` (edits, comments, form filling), `src/info.rs` (links, contents, comments and form fields read from the file), `src/sign.rs` (signatures, with the cryptography in `src/sign_windows.rs` and `src/sign_openssl.rs`), `src/print_windows.rs` and `src/print_linux.rs`, `src/register_windows.rs` and `src/register_linux.rs`, `src/fonts.rs` (finding system fonts), `src/app.rs` (the window).

## Tests

The edit tests run against any PDF with three or more pages:

```
set PROTOPDF_SAMPLE=C:\path\to\some.pdf
cargo test --release
```

On Linux: `PROTOPDF_SAMPLE=/path/to/some.pdf cargo test --release`

## Limits

- Windows and Linux. On Linux, signature cards and USB tokens cannot sign (use a .p12 / .pfx file), and whether a signer's certificate has been revoked is not checked
- Forms: calculated fields, scripts inside the form and XFA-only forms are not supported, and existing text inside the page cannot be changed
- Links to other files and attached files are not followed
- A signer counts as trusted when Windows trusts the issuer of the certificate. The EU trusted list is not consulted, so a qualified signature from an issuer whose root certificate is not installed in Windows shows as "identity could not be confirmed" even though the document check passes
- New signatures use the adbe.pkcs7.detached format with SHA-256 and the time of this computer. They do not carry a timestamp from a timestamp authority
- Password protected PDFs cannot be signed
- Saving an edited file rewrites it without encryption, so a password protected PDF loses its password once edited and saved
- Pages are printed as 300 dpi images
