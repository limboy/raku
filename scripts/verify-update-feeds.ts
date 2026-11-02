#!/usr/bin/env bun

// Fail before creating a release if the macOS feed points at a missing asset,
// uses a mutable URL, or signs bytes the shipping public key cannot verify.
import { createPublicKey, verify } from "node:crypto";
import { join } from "node:path";
import { appPublicKey, parseAppcast } from "./appcast-windows.ts";

const updateAssets = (version: string) =>
  [["appcast.xml", `Waku-${version}.zip`]] as const;

export async function verifyUpdateFeeds(
  assetsDir: string,
  version: string,
  downloadUrlPrefix: string,
  publicKey: string,
): Promise<void> {
  const key = createPublicKey({
    key: Buffer.concat([
      Buffer.from("302a300506032b6570032100", "hex"),
      Buffer.from(publicKey, "base64"),
    ]),
    format: "der",
    type: "spki",
  });
  for (const [feed, archive] of updateAssets(version)) {
    const xml = await Bun.file(join(assetsDir, feed)).text();
    const items = parseAppcast(xml).filter((item) => item.version === version);
    if (items.length !== 1) {
      throw new Error(`${feed} must contain one signed item for ${version}`);
    }
    const item = items[0]!;
    if (item.url !== `${downloadUrlPrefix}${archive}`) {
      throw new Error(
        `${feed} does not point to this GitHub release's ${archive}`,
      );
    }
    const bytes = await Bun.file(join(assetsDir, archive)).arrayBuffer();
    if (item.length !== bytes.byteLength) {
      throw new Error(`${feed} declares the wrong length for ${archive}`);
    }
    if (
      !verify(
        null,
        Buffer.from(bytes),
        key,
        Buffer.from(item.signature, "base64"),
      )
    ) {
      throw new Error(`${feed} has an invalid signature for ${archive}`);
    }
    console.log(`Verified ${feed} → ${archive}`);
  }
}

if (import.meta.main) {
  const [assetsDir, version] = process.argv.slice(2);
  const prefix = process.env.WAKU_DOWNLOAD_URL_PREFIX;
  if (!assetsDir || !version || !prefix) {
    throw new Error(
      "usage: WAKU_DOWNLOAD_URL_PREFIX=<release-url> bun scripts/verify-update-feeds.ts <assets-dir> <version>",
    );
  }
  await verifyUpdateFeeds(assetsDir, version, prefix, await appPublicKey());
}
