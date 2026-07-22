import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

async function text(path: string) {
  return readFile(resolve(process.cwd(), path), "utf-8");
}

describe("remote server deployment artifacts", () => {
  it("builds a non-root server image with a persistent data root", async () => {
    const dockerfile = await text("Dockerfile.server");

    expect(dockerfile).toMatch(/cargo build[^\n]+--release[^\n]+image-annotation-server/);
    expect(dockerfile).toContain("EXPOSE 17311");
    expect(dockerfile).toMatch(/USER\s+(?!root\b)\S+/);
    expect(dockerfile).toContain("IMAGE_ANNOTATION_DATA_DIR=/data");
    expect(dockerfile).toContain("image-annotation-server");
  });

  it("requires environment credentials and publishes a health-checked persistent service", async () => {
    const compose = await text("docker-compose.server.yml");

    expect(compose).toMatch(/17311:17311/);
    expect(compose).toMatch(/server-data:\/data/);
    expect(compose).toContain("IMAGE_ANNOTATION_ADMIN_TOKEN");
    expect(compose).toContain("${IMAGE_ANNOTATION_ADMIN_TOKEN:?");
    expect(compose).toContain("IMAGE_ANNOTATION_ALLOWED_ORIGINS");
    expect(compose).toContain("/api/v1/health");
    expect(compose).not.toMatch(/Bearer\s+[A-Za-z0-9_-]{32,}/);
  });

  it("ships an empty secret template and HTTPS reverse proxy example", async () => {
    const environment = await text(".env.server.example");
    const caddy = await text("deploy/Caddyfile.example");

    expect(environment).toContain("IMAGE_ANNOTATION_ADMIN_TOKEN=");
    expect(environment).not.toMatch(/IMAGE_ANNOTATION_ADMIN_TOKEN=\S{32,}/);
    expect(caddy).toContain("reverse_proxy image-annotation-server:17311");
  });
});
