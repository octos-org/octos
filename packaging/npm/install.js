#!/usr/bin/env node
// postinstall: download the prebuilt octos release bundle for this platform,
// extract every binary into vendor/, and assert the full skill set is present.
//
// The bundle ships `octos` alongside its 8 skill binaries. At `octos serve`
// startup, bootstrap discovers those skills as SIBLINGS of the resolved
// `octos` executable, so they must all land in the same dir (vendor/).
//
// Escapes:
//   OCTOS_SKIP_DOWNLOAD=1   skip the download entirely (CI / offline installs)
//   OCTOS_BUNDLE_URL=<url>  override the download URL (file:// supported for tests)

"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");
const https = require("https");
const http = require("http");
const { spawnSync } = require("child_process");
const { URL } = require("url");

const VENDOR_DIR = path.join(__dirname, "vendor");
const REPO = "octos-org/octos";

// Every binary the release bundle is expected to contain. `octos` is the
// server; the rest are the bundled skills discovered as siblings at runtime.
const EXPECTED_BINS = [
  "octos",
  "news_fetch",
  "deep-search",
  "deep_crawl",
  "send_email",
  "account_manager",
  "voice",
  "clock",
  "weather",
];

function fail(msg) {
  console.error("\n[@octos-org/octos] install failed: " + msg + "\n");
  process.exit(1);
}

// Map this Node platform/arch to the release triple + archive extension.
// Mirrors scripts/install.sh platform detection.
function resolveTarget() {
  const platform = os.platform();
  const arch = os.arch();

  if (platform === "darwin") {
    if (arch === "arm64") {
      return { triple: "aarch64-apple-darwin", ext: "tar.gz" };
    }
    fail(
      "octos requires Apple Silicon on macOS; no x86_64 macOS build is published."
    );
  }
  if (platform === "linux") {
    if (arch === "x64") {
      return { triple: "x86_64-unknown-linux-gnu", ext: "tar.gz" };
    }
    if (arch === "arm64") {
      return { triple: "aarch64-unknown-linux-gnu", ext: "tar.gz" };
    }
    fail("Unsupported Linux architecture: " + arch + " (need x64 or arm64).");
  }
  if (platform === "win32") {
    if (arch === "x64") {
      return { triple: "x86_64-pc-windows-msvc", ext: "zip" };
    }
    fail("Unsupported Windows architecture: " + arch + " (need x64).");
  }
  fail("Unsupported platform: " + platform);
}

// Resolve the release tag from the package version. CI sets the package
// version to the released tag (minus the leading "v"), so the tag is
// "v" + version. The "0.0.0-managed" placeholder is never published.
function resolveTag() {
  const version = require("./package.json").version;
  if (version === "0.0.0-managed") {
    fail(
      "package version is the unmanaged placeholder (0.0.0-managed); " +
        "this build was not produced by the publish workflow. " +
        "Set OCTOS_BUNDLE_URL to install manually."
    );
  }
  return version.startsWith("v") ? version : "v" + version;
}

function bundleUrl(target) {
  if (process.env.OCTOS_BUNDLE_URL) {
    return process.env.OCTOS_BUNDLE_URL;
  }
  const tag = resolveTag();
  return (
    "https://github.com/" +
    REPO +
    "/releases/download/" +
    tag +
    "/octos-bundle-" +
    target.triple +
    "." +
    target.ext
  );
}

// Download to a file, following redirects, honoring HTTPS_PROXY when set.
function download(urlStr, destFile, redirects, cb) {
  if (redirects > 10) {
    return cb(new Error("too many redirects"));
  }

  let url;
  try {
    url = new URL(urlStr);
  } catch (e) {
    return cb(new Error("invalid URL: " + urlStr));
  }

  // file:// override — copy locally (used by tests / offline installs).
  // Use fileURLToPath so Windows `file:///C:/...` maps correctly (pathname
  // would yield `/C:/...`).
  if (url.protocol === "file:") {
    try {
      fs.copyFileSync(require("url").fileURLToPath(url), destFile);
      return cb(null);
    } catch (e) {
      return cb(new Error("could not read " + urlStr + ": " + e.message));
    }
  }

  // Direct request only. Corporate HTTP/HTTPS proxies are NOT supported here
  // (Node core has no CONNECT helper, and absolute-form GET to an HTTP proxy
  // fails for https targets). Behind a proxy: pre-download the bundle and point
  // the installer at it with OCTOS_BUNDLE_URL=file:///path, or set
  // OCTOS_SKIP_DOWNLOAD=1 and place the binaries under vendor/ yourself.
  const transport = url.protocol === "https:" ? https : http;
  const requestOptions = {
    protocol: url.protocol,
    hostname: url.hostname,
    port: url.port,
    path: url.pathname + url.search,
    headers: { "User-Agent": "octos-npm-installer" },
  };

  transport
    .get(requestOptions, (res) => {
      // Follow redirects (GitHub release assets redirect to a CDN).
      if (
        res.statusCode >= 300 &&
        res.statusCode < 400 &&
        res.headers.location
      ) {
        res.resume();
        const next = new URL(res.headers.location, urlStr).toString();
        return download(next, destFile, redirects + 1, cb);
      }
      if (res.statusCode !== 200) {
        res.resume();
        return cb(
          new Error("HTTP " + res.statusCode + " fetching " + urlStr)
        );
      }
      const out = fs.createWriteStream(destFile);
      res.pipe(out);
      out.on("finish", () => out.close(() => cb(null)));
      out.on("error", (e) => cb(e));
    })
    .on("error", (e) => cb(e));
}

// Extract the archive into vendor/ using the platform's native tool.
function extract(archiveFile, ext) {
  fs.mkdirSync(VENDOR_DIR, { recursive: true });
  let res;
  if (ext === "zip") {
    if (os.platform() === "win32") {
      res = spawnSync(
        "powershell",
        [
          "-NoProfile",
          "-Command",
          "Expand-Archive -Force -LiteralPath '" +
            archiveFile +
            "' -DestinationPath '" +
            VENDOR_DIR +
            "'",
        ],
        { stdio: "inherit" }
      );
    } else {
      res = spawnSync("unzip", ["-o", archiveFile, "-d", VENDOR_DIR], {
        stdio: "inherit",
      });
    }
  } else {
    res = spawnSync("tar", ["-xzf", archiveFile, "-C", VENDOR_DIR], {
      stdio: "inherit",
    });
  }
  if (res.error) {
    fail("extraction tool failed to launch: " + res.error.message);
  }
  if (res.status !== 0) {
    fail("extraction exited with status " + res.status);
  }
}

// Make all extracted binaries executable and assert the full set is present.
function finalizeAndVerify() {
  const exeSuffix = os.platform() === "win32" ? ".exe" : "";
  const missing = [];
  for (const name of EXPECTED_BINS) {
    const p = path.join(VENDOR_DIR, name + exeSuffix);
    if (fs.existsSync(p)) {
      try {
        fs.chmodSync(p, 0o755);
      } catch (e) {
        // Best-effort on Windows where chmod is largely a no-op.
      }
    } else {
      missing.push(name + exeSuffix);
    }
  }
  if (missing.length > 0) {
    fail(
      "the downloaded bundle is missing expected binaries: " +
        missing.join(", ") +
        ". The release archive may be corrupt or incomplete."
    );
  }
}

function main() {
  if (process.env.OCTOS_SKIP_DOWNLOAD === "1") {
    console.log(
      "[@octos-org/octos] OCTOS_SKIP_DOWNLOAD=1 set; skipping bundle download."
    );
    return;
  }

  const target = resolveTarget();
  const url = bundleUrl(target);
  console.log("[@octos-org/octos] downloading " + url);

  const tmpDir = fs.mkdtempSync(path.join(os.tmpdir(), "octos-npm-"));
  const archiveFile = path.join(tmpDir, "bundle." + target.ext);

  download(url, archiveFile, 0, (err) => {
    if (err) {
      fail(err.message);
    }
    extract(archiveFile, target.ext);
    finalizeAndVerify();
    try {
      fs.rmSync(tmpDir, { recursive: true, force: true });
    } catch (e) {
      // non-fatal cleanup failure
    }
    console.log(
      "[@octos-org/octos] installed octos + " +
        (EXPECTED_BINS.length - 1) +
        " bundled skills into vendor/"
    );
  });
}

main();
