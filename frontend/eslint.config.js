// ESLint flat config: JS recommended + typescript-eslint (non type-checked)
// + eslint-plugin-solid. eslint-config-prettier comes last to switch off any
// stylistic rule that would fight Prettier (which runs as a separate tool).
import js from '@eslint/js';
import tseslint from 'typescript-eslint';
import solid from 'eslint-plugin-solid/configs/typescript';
import prettier from 'eslint-config-prettier';

export default tseslint.config(
  js.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['**/*.{ts,tsx}'],
    ...solid,
  },
  prettier,
);
