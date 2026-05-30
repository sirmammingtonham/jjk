import { defineConfig } from 'vocs/config'

export default defineConfig({
  title: 'jjk',
  titleTemplate: '%s – jjk',
  description:
    'Familiar git commands and a stacking workflow over Jujutsu (jj) for painless stacked GitHub PRs.',
  // GitHub Pages serves the repo at https://sirmammingtonham.github.io/jjk/
  baseUrl: 'https://sirmammingtonham.github.io/jjk',
  basePath: '/jjk',
  // Pure static bundle (no server runtime) for GitHub Pages.
  renderStrategy: 'full-static',
  theme: {
    accentColor: '#7c5cff',
  },
  topNav: [
    { text: 'Quickstart', link: '/quickstart' },
    { text: 'Commands', link: '/commands' },
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
        { text: 'The stacked-PR workflow', link: '/workflow' },
        { text: 'Command reference', link: '/commands' },
      ],
    },
  ],
})
