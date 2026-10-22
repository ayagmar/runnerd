import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://ayagmar.github.io',
  base: '/runnerd',
  integrations: [
    starlight({
      title: 'runnerd',
      social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/ayagmar/runnerd' }],
    }),
  ],
});
