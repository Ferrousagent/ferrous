# Ferrous Design Tokens — Linear Dark

> Preserved from the removed frontend scaffold (`src/index.css`). These tokens were hand-picked to match the Linear-inspired direction (deep dark canvas, lavender accent, Geist type). Re-apply when the Tauri UI work starts.

## Theme summary

- **Dark-only**, `color-scheme: dark` on `:root` and `.dark`.
- Deep near-black canvas with a faint blue tint (`#010102`), NOT pure `#000000`.
- Single chromatic accent: Linear lavender `#5e6ad2`, used sparingly (brand, focus, primary CTA).
- Hierarchy via surface ladder (canvas → card → secondary/muted), hairline borders, no heavy shadows.
- Fonts: **Geist Variable** (sans) + **Geist Mono Variable** via `@fontsource-variable/*`.

## CSS variables (copy as-is)

```css
:root, .dark {
  color-scheme: dark;
  --background: #010102;
  --foreground: #f7f8f8;
  --card: #0f1011;
  --card-foreground: #f7f8f8;
  --popover: #18191a;
  --popover-foreground: #f7f8f8;
  --primary: #5e6ad2;
  --primary-foreground: #ffffff;
  --secondary: #141516;
  --secondary-foreground: #d0d6e0;
  --muted: #141516;
  --muted-foreground: #8a8f98;
  --accent: #141516;
  --accent-foreground: #f7f8f8;
  --destructive: #e5484d;
  --destructive-foreground: #ffffff;
  --border: #23252a;
  --input: #23252a;
  --ring: #5e6ad2;
  --radius: 0.75rem;
  --success: #27a644;
  --chart-1: #5e6ad2;
  --chart-2: #828fff;
  --chart-3: #7a7fad;
  --chart-4: #8a8f98;
  --chart-5: #3f46a8;
  --sidebar: #0f1011;
  --sidebar-foreground: #f7f8f8;
  --sidebar-primary: #5e6ad2;
  --sidebar-primary-foreground: #ffffff;
  --sidebar-accent: #141516;
  --sidebar-accent-foreground: #d0d6e0;
  --sidebar-border: #23252a;
  --sidebar-ring: #5e6ad2;
}
```

## Tailwind v4 theme mapping

```css
@import "tailwindcss";
@import "tw-animate-css";
@import "shadcn/tailwind.css";
@import "@fontsource-variable/geist";
@import "@fontsource-variable/geist-mono";

@custom-variant dark (&:is(.dark *));

@theme inline {
  /* map every --color-* to the vars above (background, foreground, card,
     popover, primary, secondary, muted, accent, destructive, border, input,
     ring, success, sidebar-*, chart-1..5) */
  --font-heading: var(--font-sans);
  --font-sans: "Geist Variable", sans-serif;
  --font-mono: "Geist Mono Variable", ui-monospace, SFMono-Regular, Menlo, monospace;
  --radius-sm: calc(var(--radius) - 0.25rem);
  --radius-md: calc(var(--radius) - 0.125rem);
  --radius-lg: var(--radius);
  --radius-xl: calc(var(--radius) + 0.25rem);
  --radius-2xl: calc(var(--radius) * 1.8);
  --radius-3xl: calc(var(--radius) * 2.2);
  --radius-4xl: calc(var(--radius) * 2.6);
}

@layer base {
  * { @apply border-border outline-ring/50; }
  body { @apply bg-background text-foreground; }
}
```

## Notes for the Tauri day

- Scaffold with `npm create tauri-app@latest` (gives a Tauri-correct Vite config: `clearScreen: false`, `server.port: 1420`, `strictPort: true`, `build.target: 'es2021'`).
- Then `npx shadcn@latest init` — note: the previous `components.json` used `"style": "base-nova"` which is NOT a real shadcn style (real ones: `new-york`, `default`). Let the CLI generate a fresh one.
- Icons: Phosphor (`@phosphor-icons/react`) was the chosen family. shadcn's `iconLibrary` can be set to `phosphor`.
- Fonts: Geist Variable + Geist Mono Variable (`@fontsource-variable/geist`, `@fontsource-variable/geist-mono`).
