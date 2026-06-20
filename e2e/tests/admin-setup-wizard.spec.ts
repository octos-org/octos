import { expect, test, type Page, type TestInfo } from '@playwright/test';
import { spawn, type ChildProcessWithoutNullStreams } from 'node:child_process';
import { existsSync, statSync } from 'node:fs';
import fs from 'node:fs/promises';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';

const BOOTSTRAP_TOKEN = 'octos-setup-wizard-bootstrap';
const ROTATED_TOKEN = 'Setup512';
const SERVER_READY_TIMEOUT_MS = 360_000;

const repoRoot =
  path.basename(process.cwd()) === 'e2e'
    ? path.resolve(process.cwd(), '..')
    : process.cwd();

type SpawnedServe = {
  baseURL: string;
  dataDir: string;
  proc: ChildProcessWithoutNullStreams;
  logs: string[];
};

test.setTimeout(420_000);

test('first-install admin setup wizard rotates token and persists completion', async ({
  page,
}, testInfo) => {
  const serve = await startServe(testInfo);

  try {
    await loginWithBootstrapToken(page, serve.baseURL);
    await expect(page).toHaveURL(/\/admin\/setup\/welcome$/);
    await expect(page.getByRole('heading', { name: 'Welcome to Octos' })).toBeVisible();

    await page.getByRole('link', { name: 'Get Started' }).click();
    await expect(page).toHaveURL(/\/admin\/setup\/rotate-token$/);
    await rotateAdminToken(page, serve.dataDir);

    await page.getByRole('button', { name: "I've saved it, continue" }).click();
    await expect(page).toHaveURL(/\/admin\/setup\/wizard\?step=0$/);

    await walkWizardAndAssertSmtpGating(page, serve.baseURL);

    const completedAt = await readCompletedAt(serve.dataDir);
    expect(completedAt).toBeTruthy();

    await page.getByRole('link', { name: 'Setup Wizard' }).click();
    await expect(page).toHaveURL(/\/admin\/setup\/wizard(\?step=0)?$/);
    await expect(page.getByRole('heading', { name: "What's Next" })).toBeVisible();
    await expect.poll(() => readCompletedAt(serve.dataDir)).toBe(completedAt);
  } finally {
    await attachServeLogs(testInfo, serve.logs);
    await stopServe(serve);
  }
});

async function startServe(testInfo: TestInfo): Promise<SpawnedServe> {
  const port = await freePort();
  const dataDir = await fs.mkdtemp(path.join(os.tmpdir(), 'octos-setup-wizard-e2e-'));
  const baseURL = `http://127.0.0.1:${port}`;
  const logs: string[] = [];
  const serveArgs = [
    'serve',
    '--host',
    '127.0.0.1',
    '--port',
    String(port),
    '--data-dir',
    dataDir,
    '--auth-token',
    BOOTSTRAP_TOKEN,
  ];
  const command = serveCommand(serveArgs);

  const proc = spawn(
    command.bin,
    command.args,
    {
      cwd: repoRoot,
      env: { ...process.env, RUST_LOG: process.env.RUST_LOG || 'warn' },
      stdio: ['ignore', 'pipe', 'pipe'],
    },
  );

  proc.stdout.on('data', (chunk) => logs.push(String(chunk)));
  proc.stderr.on('data', (chunk) => logs.push(String(chunk)));

  try {
    await waitForServe(baseURL, proc, logs);
  } catch (error) {
    await attachServeLogs(testInfo, logs);
    await stopServe({ baseURL, dataDir, proc, logs });
    throw error;
  }

  return { baseURL, dataDir, proc, logs };
}

function serveCommand(serveArgs: string[]): { bin: string; args: string[] } {
  const configured = process.env.OCTOS_SETUP_WIZARD_BIN;
  const candidates = [
    configured,
    path.join(repoRoot, 'target', 'release', 'octos'),
    path.join(repoRoot, 'target', 'debug', 'octos'),
  ].filter((candidate): candidate is string => Boolean(candidate));
  const binary = candidates.find((candidate) => existsSync(candidate));
  if (binary) return { bin: binary, args: serveArgs };
  return {
    bin: 'cargo',
    args: ['run', '-p', 'octos-cli', '--features', 'api', '--', ...serveArgs],
  };
}

async function waitForServe(
  baseURL: string,
  proc: ChildProcessWithoutNullStreams,
  logs: string[],
): Promise<void> {
  const deadline = Date.now() + SERVER_READY_TIMEOUT_MS;
  let exitCode: number | null = null;
  proc.once('exit', (code) => {
    exitCode = code;
  });

  while (Date.now() < deadline) {
    if (exitCode !== null) {
      throw new Error(`octos serve exited early with code ${exitCode}\n${logs.join('')}`);
    }
    try {
      const response = await fetch(`${baseURL}/admin/login`);
      if (response.ok) return;
    } catch {
      // Server is still compiling or binding.
    }
    await sleep(500);
  }

  throw new Error(`octos serve did not become ready at ${baseURL}\n${logs.join('')}`);
}

async function loginWithBootstrapToken(page: Page, baseURL: string) {
  await page.goto(`${baseURL}/admin/login`, { waitUntil: 'domcontentloaded' });
  await page.getByTestId('admin-token-tab').click();
  await page.getByTestId('token-input').fill(BOOTSTRAP_TOKEN);
  await page.getByTestId('login-button').click();
}

async function rotateAdminToken(page: Page, dataDir: string) {
  await page.locator('#admin-token-input').fill(ROTATED_TOKEN);
  await page.getByRole('button', { name: 'Submit' }).click();
  await expect(page.getByRole('button', { name: "I've saved it, continue" })).toBeVisible();

  const adminTokenPath = path.join(dataDir, 'admin_token.json');
  await expect.poll(() => existsSync(adminTokenPath)).toBe(true);
  expect(statSync(adminTokenPath).mode & 0o777).toBe(0o600);
}

async function walkWizardAndAssertSmtpGating(page: Page, baseURL: string) {
  await expect(page.getByRole('heading', { name: "What's Next" })).toBeVisible();
  await page.getByRole('button', { name: 'Next' }).click();

  await expect(page.getByRole('heading', { name: 'LLM Provider' })).toBeVisible();
  await page.getByRole('button', { name: 'Next' }).click();

  await expect(page.getByRole('heading', { name: 'System Email (SMTP)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Save and Continue' })).toBeEnabled();
  await page.getByRole('button', { name: 'Save and Continue' }).click();

  await expect(page.getByRole('heading', { name: 'Deployment Mode' })).toBeVisible();
  const modeSave = page.waitForResponse(
    (response) =>
      response.url().endsWith('/api/admin/deployment-mode') &&
      response.request().method() === 'POST' &&
      response.status() === 204,
  );
  await page.getByLabel('Tenant').check();
  await modeSave;
  await expect(page.locator('input[name="deployment-mode"][value="tenant"]')).toBeChecked();
  await page.getByRole('button', { name: 'Back' }).click();

  await expect(page.getByRole('heading', { name: 'System Email (SMTP)' })).toBeVisible();
  await expect(page.getByRole('button', { name: 'Save and Continue' })).toBeDisabled();

  await page.getByLabel('Host').fill('127.0.0.1');
  await page.getByLabel('Port').fill('1');
  await page.getByLabel('From address').fill('noreply@example.com');
  await page.getByLabel('Username').fill('smtp-user');
  await page.getByLabel(/^Password/).fill('smtp-pass');
  await expect(page.getByRole('button', { name: 'Save and Continue' })).toBeEnabled();

  await page.getByLabel('Send test email to').fill('operator@example.com');
  const responsePromise = page.waitForResponse(
    (response) => response.url().endsWith('/api/admin/smtp/test') && response.status() === 200,
  );
  await page.getByRole('button', { name: 'Send Test' }).click();
  const response = await responsePromise;
  const body = (await response.json()) as { ok?: boolean; message?: string; error?: string };
  expect(typeof body.ok).toBe('boolean');
  const resultText = body.message || body.error;
  expect(resultText).toBeTruthy();
  await expect(page.getByText(resultText!, { exact: false })).toBeVisible();

  await page.getByRole('button', { name: 'Save and Continue' }).click();
  await expect(page.getByRole('heading', { name: 'Deployment Mode' })).toBeVisible();
  await page.getByRole('button', { name: 'Next' }).click();

  await expect(page.getByRole('heading', { name: 'All set' })).toBeVisible();
  await page.getByRole('button', { name: /Go to Dashboard/ }).click();
  await expect(page).toHaveURL(`${baseURL}/admin/`);
}

async function readCompletedAt(dataDir: string): Promise<string | null> {
  const raw = await fs.readFile(path.join(dataDir, 'setup_state.json'), 'utf8');
  const state = JSON.parse(raw) as { wizard_completed_at?: string | null };
  return state.wizard_completed_at ?? null;
}

async function stopServe(serve: SpawnedServe) {
  if (serve.proc.exitCode === null && serve.proc.signalCode === null) {
    serve.proc.kill('SIGTERM');
    await Promise.race([
      new Promise((resolve) => serve.proc.once('exit', resolve)),
      sleep(5_000).then(() => {
        if (serve.proc.exitCode === null && serve.proc.signalCode === null) {
          serve.proc.kill('SIGKILL');
        }
      }),
    ]);
  }
  await fs.rm(serve.dataDir, { recursive: true, force: true });
}

async function attachServeLogs(testInfo: TestInfo, logs: string[]) {
  await testInfo.attach('octos-serve.log', {
    body: logs.join(''),
    contentType: 'text/plain',
  });
}

async function freePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const address = server.address();
      if (!address || typeof address === 'string') {
        server.close(() => reject(new Error('failed to allocate TCP port')));
        return;
      }
      const port = address.port;
      server.close(() => resolve(port));
    });
  });
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
