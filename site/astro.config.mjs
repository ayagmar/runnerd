import { defineConfig, fontProviders } from 'astro/config';
import starlight from '@astrojs/starlight';

const site = 'https://ayagmar.github.io';
const base = '/runnerd';

// The social card: a screenshot of the landing page in a 1200x630 viewport at 1x, dark theme, 9s into the replay.
const card = [
  { tag: 'meta', attrs: { property: 'og:image', content: `${site}${base}/og.png` } },
  { tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
  { tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
  { tag: 'meta', attrs: { name: 'twitter:image', content: `${site}${base}/og.png` } },
];

export default defineConfig({
  site,
  base,
  trailingSlash: 'always',
  // Source Serif 4 and Libre Franklin (OFL, public/fonts), Fontsource's latin files cut to ASCII and the punctuation the
  // site uses: pyftsubset --unicodes=U+20-7E,U+A0,U+B7,U+D7,U+2013,U+2014,U+2018,U+2019,U+201C,U+201D,U+2026,U+2212
  // --layout-features='*' --flavor=woff2. The serif's cut is renamed runnerd serif in its name table, since 'Source' is
  // a Reserved Font Name. Astro emits metric-matched fallbacks, so the swap moves nothing.
  fonts: [
    {
      provider: fontProviders.local(),
      name: 'runnerd serif',
      cssVariable: '--font-text',
      fallbacks: ['Georgia', 'serif'],
      options: {
        variants: [
          { src: ['./src/assets/fonts/runnerd-serif-normal.woff2'], weight: '200 900', style: 'normal' },
          { src: ['./src/assets/fonts/runnerd-serif-italic.woff2'], weight: '200 900', style: 'italic' },
        ],
      },
    },
    {
      provider: fontProviders.local(),
      name: 'Libre Franklin',
      cssVariable: '--font-display',
      fallbacks: ['Arial', 'sans-serif'],
      options: {
        variants: [{ src: ['./src/assets/fonts/libre-franklin-text-normal.woff2'], weight: '100 900', style: 'normal' }],
      },
    },
  ],
  integrations: [
    starlight({
      title: 'runnerd',
      logo: { src: './src/assets/mark.svg', alt: '' },
      favicon: '/favicon.svg',
      components: { Head: './src/components/Head.astro' },
      customCss: ['./src/styles/theme.css'],
      head: card,
      expressiveCode: { defaultProps: { wrap: true } },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/ayagmar/runnerd' }],
      sidebar: [{ label: 'Start here', items: ['install', 'configuration', 'admission', 'running', 'clients', 'upgrading'] }],
    }),
  ],
});
