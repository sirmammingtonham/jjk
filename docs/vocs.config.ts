import { defineConfig } from 'vocs/config'

export default defineConfig({
  title: 'jjk',
  titleTemplate: '%s – jjk',
  description:
    'Familiar git commands and a stacking workflow over Jujutsu (jj) for painless stacked GitHub PRs.',
  baseUrl: 'https://ethan.website/jjk',
  basePath: '/jjk',
  // basePath isn't auto-prepended to iconUrl (unlike vocs's own assets), so include it.
  iconUrl: '/jjk/favicon.ico',
  // Pure static bundle (no server runtime) for GitHub Pages.
  renderStrategy: 'full-static',
  theme: {
    accentColor: '#7c5cff',
  },
  topNav: [
    { text: 'Quickstart', link: '/quickstart' },
    { text: 'Commands', link: '/commands' },
    { text: 'Domain Expansion', link: '/domain-expansion' },
    { text: 'jj', link: 'https://jj-vcs.github.io/jj/' },
  ],
  socials: [{ icon: 'github', link: 'https://github.com/sirmammingtonham/jjk' }],
  sidebar: [
    {
      text: 'Introduction',
      collapsed: false,
      items: [
        { text: 'What is jjk?', link: '/' },
        { text: 'Installation', link: '/installation' },
        { text: 'Quickstart', link: '/quickstart' },
      ],
    },
    {
      text: 'Guide',
      collapsed: false,
      items: [
        { text: 'Stacked-PR workflow (manual)', link: '/workflow' },
        { text: 'Domain Expansion (automatic)', link: '/domain-expansion' },
        { text: 'Command reference', link: '/commands' },
      ],
    },
  ],
})
