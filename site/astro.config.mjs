import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://ayagmar.github.io',
  base: '/runnerd',
  integrations: [
    starlight({
      title: 'runnerd',
      logo: { src: './src/assets/mark.svg', alt: '' },
      favicon: '/favicon.svg',
      customCss: ['./src/styles/theme.css'],
      expressiveCode: { defaultProps: { wrap: true } },
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/ayagmar/runnerd' }],
      sidebar: [{ label: 'Start here', items: ['install', 'configuration', 'running'] }],
    }),
  ],
});
