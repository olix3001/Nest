// @ts-check
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

const nestGrammar = JSON.parse(
	readFileSync(fileURLToPath(new URL('./src/nest/nest.tmLanguage.json', import.meta.url)), 'utf-8'),
);

// https://astro.build/config
export default defineConfig({
	integrations: [
		starlight({
			title: 'Nest',
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/olix3001/Nest' }],
			expressiveCode: {
				shiki: {
					langs: [nestGrammar],
				},
			},
			sidebar: [
				{
					label: 'Getting Started',
					items: [{ label: 'Installation & first program', slug: 'getting-started' }],
				},
				{
					label: 'The Language',
					items: [
						{ label: 'Bindings', slug: 'language/bindings' },
						{ label: 'Types', slug: 'language/types' },
						{ label: 'Structs', slug: 'language/structs' },
						{ label: 'Enums', slug: 'language/enums' },
						{ label: 'Functions', slug: 'language/functions' },
						{ label: 'Closures and Func', slug: 'language/closures' },
						{ label: 'Traits and impls', slug: 'language/traits' },
						{ label: 'Operators', slug: 'language/operators' },
						{ label: 'Control flow', slug: 'language/control-flow' },
						{ label: 'Errors', slug: 'language/errors' },
						{ label: 'Namespaces and packages', slug: 'language/namespaces' },
						{ label: 'Memory', slug: 'language/memory' },
						{ label: 'Directives and attributes', slug: 'language/directives' },
						{ label: 'C FFI', slug: 'language/c-ffi' },
						{ label: 'Testing', slug: 'language/testing' },
					],
				},
				{
					label: 'Toolchain',
					items: [{ label: 'nestc and twig', slug: 'toolchain' }],
				},
			],
		}),
	],
});
