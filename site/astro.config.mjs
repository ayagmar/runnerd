import { defineConfig, fontProviders } from 'astro/config';
import starlight from '@astrojs/starlight';

// Source Serif 4 and Libre Franklin (OFL, public/fonts), Fontsource's latin files cut to ASCII and the punctuation the
// site uses: pyftsubset --unicodes=U+20-7E,U+A0,U+B7,U+D7,U+2013,U+2014,U+2018,U+2019,U+201C,U+201D,U+2026,U+2192,U+2212
// --layout-features='*' --flavor=woff2. Astro emits metric-matched fallbacks, so the swap moves nothing.
export default defineConfig({
  site: 'https://ayagmar.github.io',
  base: '/runnerd',
  trailingSlash: 'always',
  fonts: [
    {
      provider: fontProviders.local(),
      name: 'Source Serif 4',
      cssVariable: '--font-text',
      fallbacks: ['Georgia', 'serif'],
      options: {
        variants: [
          { src: ['./src/assets/fonts/source-serif-4-text-normal.woff2'], weight: '200 900', style: 'normal' },
          { src: ['./src/assets/fonts/source-serif-4-text-italic.woff2'], weight: '200 900', style: 'italic' },
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
      expressiveCode: { defaultProps: { wrap: true } },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/ayagmar/runnerd' }],
      sidebar: [{ label: 'Start here', items: ['install', 'configuration', 'running', 'clients'] }],
    }),
  ],
});
