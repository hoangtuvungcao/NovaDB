import { defineConfig } from 'vitepress'

const base = '/NovaDB/'
const repo = 'https://github.com/hoangtuvungcao/NovaDB'

export default defineConfig({
  base,
  lang: 'en-US',
  title: 'NovaDB',
  description:
    'Local-first SQL database built on SQLite: HTTP API, Web Studio, PostgreSQL and MySQL wire gateways, LWW sync, vector functions.',
  cleanUrls: true,
  lastUpdated: true,
  // Links into the repository (../crates, ../examples, ...) are absolute GitHub URLs.
  ignoreDeadLinks: [/^https?:\/\/localhost/, /^http:\/\/127\.0\.0\.1/],

  head: [
    ['link', { rel: 'icon', type: 'image/svg+xml', href: `${base}logo-mark.svg` }],
    ['meta', { name: 'theme-color', content: '#5B4BF5' }],
    ['meta', { property: 'og:type', content: 'website' }],
    ['meta', { property: 'og:title', content: 'NovaDB' }],
    ['meta', { property: 'og:description', content: 'Local-first SQL database. One file. Any client.' }],
  ],

  markdown: {
    theme: { light: 'github-light', dark: 'github-dark' },
  },

  themeConfig: {
    logo: { light: '/logo-mark.svg', dark: '/logo-mark.svg', alt: 'NovaDB' },
    siteTitle: 'NovaDB',

    nav: [
      { text: 'Get started', link: '/getting-started/quickstart', activeMatch: '/getting-started/' },
      { text: 'Guides', link: '/guides/cli-console', activeMatch: '/(guides|sdks)/' },
      { text: 'Connect', link: '/connect/http-api', activeMatch: '/connect/' },
      { text: 'Migrate', link: '/migration/', activeMatch: '/migration/' },
      { text: 'Deploy', link: '/deployment/binary-systemd', activeMatch: '/deployment/' },
      { text: 'Reference', link: '/reference/sql-dialect', activeMatch: '/reference/' },
      {
        text: 'v0.2.1',
        items: [
          { text: 'Changelog', link: `${repo}/blob/master/CHANGELOG.md` },
          { text: 'Releases', link: `${repo}/releases` },
          { text: 'Contributing', link: `${repo}/blob/master/CONTRIBUTING.md` },
        ],
      },
    ],

    sidebar: {
      '/': [
        {
          text: 'Getting started',
          items: [
            { text: 'What is NovaDB?', link: '/getting-started/introduction' },
            { text: 'Installation', link: '/getting-started/installation' },
            { text: 'Quickstart (5 minutes)', link: '/getting-started/quickstart' },
          ],
        },
        {
          text: 'Guides',
          items: [
            { text: 'CLI and console', link: '/guides/cli-console' },
            { text: 'Web Studio', link: '/guides/web-studio' },
            { text: 'Sync (local-first)', link: '/guides/sync' },
            { text: 'Vector search', link: '/guides/vector-search' },
            { text: 'Authentication and RBAC', link: '/guides/rbac' },
            { text: 'Backups and recovery', link: '/guides/backups' },
            { text: 'Game and multi-user servers', link: '/guides/game-servers' },
          ],
        },
        {
          text: 'SDKs',
          items: [
            { text: 'Overview', link: '/sdks/' },
            { text: 'JavaScript / TypeScript', link: '/sdks/javascript' },
            { text: 'Python', link: '/sdks/python' },
            { text: 'C# / .NET / Unity', link: '/sdks/csharp' },
            { text: 'Go', link: '/sdks/go' },
          ],
        },
        {
          text: 'Connect',
          items: [
            { text: 'HTTP API', link: '/connect/http-api' },
            { text: 'PostgreSQL wire', link: '/connect/postgresql' },
            { text: 'MySQL wire (new)', link: '/connect/mysql' },
            { text: 'ORM and driver compatibility', link: '/connect/orm-compatibility' },
          ],
        },
        {
          text: 'Migration guides',
          items: [
            { text: 'Overview and compatibility', link: '/migration/' },
            { text: 'From SQLite', link: '/migration/from-sqlite' },
            { text: 'From MySQL', link: '/migration/from-mysql' },
            { text: 'From SQL Server', link: '/migration/from-sql-server' },
            { text: 'From PostgreSQL', link: '/migration/from-postgresql' },
          ],
        },
        {
          text: 'Deployment',
          items: [
            { text: 'Binary and systemd', link: '/deployment/binary-systemd' },
            { text: 'Docker and Compose', link: '/deployment/docker' },
            { text: 'Kubernetes', link: '/deployment/kubernetes' },
            { text: 'Security hardening', link: '/deployment/security' },
          ],
        },
        {
          text: 'Reference',
          items: [
            { text: 'SQL dialect', link: '/reference/sql-dialect' },
            { text: 'Functions', link: '/reference/functions' },
            { text: 'CLI', link: '/reference/cli' },
            { text: 'Configuration and env vars', link: '/reference/configuration' },
            { text: 'HTTP API', link: '/reference/http-api' },
          ],
        },
        {
          text: 'More',
          items: [{ text: 'FAQ and limitations', link: '/faq' }],
        },
      ],
    },

    search: { provider: 'local' },

    socialLinks: [{ icon: 'github', link: repo }],

    editLink: {
      pattern: `${repo}/edit/master/website/:path`,
      text: 'Edit this page on GitHub',
    },

    footer: {
      message: 'Released under the Apache-2.0 License.',
      copyright: 'Copyright NovaDB contributors',
    },

    outline: { level: [2, 3] },
  },
})
