import { afterEach, expect, test } from "bun:test";
import { generateKeyPairSync, sign } from "node:crypto";
import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { verifyUpdateFeeds } from "./verify-update-feeds.ts";

const version = "1.2.3";
const prefix = `https://github.com/limboy/raku/releases/download/v${version}/`;
const directories: string[] = [];

afterEach(async () => {
  await Promise.all(
    directories
      .splice(0)
      .map((dir) => rm(dir, { recursive: true, force: true })),
  );
});

async function releaseFixture() {
  const directory = await mkdtemp(join(tmpdir(), "raku-update-feeds-"));
  directories.push(directory);
  const { publicKey, privateKey } = generateKeyPairSync("ed25519");
  const publicKeyBase64 = Buffer.from(
    publicKey.export({ format: "jwk" }).x!,
    "base64url",
  ).toString("base64");
  const archive = `Raku-${version}.zip`;
  const bytes = Buffer.from("macOS release archive");
  const signature = sign(null, bytes, privateKey).toString("base64");
  // Sparkle uses an internal build number, distinct from the public version.
  const xml = `<?xml version="1.0"?>
<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle" version="2.0">
  <channel><item>
    <sparkle:version>1002003</sparkle:version>
    <sparkle:shortVersionString>${version}</sparkle:shortVersionString>
    <enclosure url="${prefix}${archive}" length="${bytes.length}" type="application/octet-stream" sparkle:edSignature="${signature}" />
  </item></channel>
</rss>`;
  await Bun.write(join(directory, archive), bytes);
  await Bun.write(join(directory, "appcast.xml"), xml);
  return { directory, publicKey: publicKeyBase64 };
}

test("the signed macOS update feed verifies without other platform assets", async () => {
  const fixture = await releaseFixture();
  await verifyUpdateFeeds(
    fixture.directory,
    version,
    prefix,
    fixture.publicKey,
  );
});

test(
  "rejects archive tampering even when the byte count is unchanged",
  async () => {
    const fixture = await releaseFixture();
    const path = join(fixture.directory, `Raku-${version}.zip`);
    const bytes = Buffer.from(await Bun.file(path).arrayBuffer());
    bytes[0] = bytes[0]! ^ 1;
    await Bun.write(path, bytes);
    await expect(
      verifyUpdateFeeds(fixture.directory, version, prefix, fixture.publicKey),
    ).rejects.toThrow("invalid signature");
  },
);

test("rejects an archive URL from a different release", async () => {
  const fixture = await releaseFixture();
  const path = join(fixture.directory, "appcast.xml");
  await Bun.write(
    path,
    (await Bun.file(path).text()).replace(prefix, "https://releases.waku.sh/"),
  );
  await expect(
    verifyUpdateFeeds(fixture.directory, version, prefix, fixture.publicKey),
  ).rejects.toThrow("does not point to this GitHub release");
});

test("rejects a truncated download", async () => {
  const fixture = await releaseFixture();
  await Bun.write(join(fixture.directory, `Raku-${version}.zip`), "truncated");
  await expect(
    verifyUpdateFeeds(fixture.directory, version, prefix, fixture.publicKey),
  ).rejects.toThrow("wrong length");
});

test("rejects a missing macOS feed", async () => {
  const fixture = await releaseFixture();
  await rm(join(fixture.directory, "appcast.xml"));
  await expect(
    verifyUpdateFeeds(fixture.directory, version, prefix, fixture.publicKey),
  ).rejects.toThrow();
});

test("rejects feeds with no signed item for this version", async () => {
  const fixture = await releaseFixture();
  const path = join(fixture.directory, "appcast.xml");
  await Bun.write(
    path,
    (await Bun.file(path).text()).replace("sparkle:edSignature=", "unsigned="),
  );
  await expect(
    verifyUpdateFeeds(fixture.directory, version, prefix, fixture.publicKey),
  ).rejects.toThrow("one signed item");
});

test("rejects archives signed by a different key", async () => {
  const fixture = await releaseFixture();
  const other = await releaseFixture();
  await expect(
    verifyUpdateFeeds(fixture.directory, version, prefix, other.publicKey),
  ).rejects.toThrow("invalid signature");
});
