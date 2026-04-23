/** @type {import('tailwindcss').Config} */
export default {
  content: ['./index.html', './src/**/*.{js,ts,jsx,tsx}'],
  theme: {
    extend: {
      colors: {
        surface: {
          DEFAULT: '#16213e',
          light: '#1a2744',
          dark: '#0f172a',
        },
        bg: {
          DEFAULT: '#1a1a2e',
          light: '#1e1e3a',
        },
        background: {
          DEFAULT: '#1a1a2e',
        },
        accent: {
          DEFAULT: '#0ea5e9',
          light: '#38bdf8',
          dark: '#0284c7',
        },
      },
    },
  },
  plugins: [],
}
