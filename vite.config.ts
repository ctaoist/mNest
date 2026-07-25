import { createRequire } from 'node:module'
import { resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

const projectRoot = fileURLToPath(new URL('.', import.meta.url))
const webRoot = resolve(projectRoot, 'web')
const requireFromWeb = createRequire(new URL('./web/package.json', import.meta.url))

async function importFromWeb(packageName: string) {
  return import(pathToFileURL(requireFromWeb.resolve(packageName)).href)
}

export default async function config() {
  const { default: solid } = await importFromWeb('vite-plugin-solid')
  const { default: tailwindcss } = await importFromWeb('@tailwindcss/vite')

  return {
    root: webRoot,
    plugins: [solid(), tailwindcss()],
    server: {
      port: 5173,
      allowedHosts: ['debug.frp.ctaoist.cn'],
      proxy: {
        '/api': 'http://127.0.0.1:4535',
        '/rest': 'http://127.0.0.1:4535',
        '/user': 'http://127.0.0.1:4535',
        '/health': 'http://127.0.0.1:4535',
      },
    },
    test: {
      environment: 'jsdom',
      setupFiles: ['./tests/setup.ts'],
    },
  }
}
