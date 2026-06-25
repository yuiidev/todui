# Syntax Themes

Put public TextMate `.tmTheme` files in this directory, then set `syntax_theme` in `../settings.json` to the file stem.

For example, `themes/Solarized Dark.tmTheme` is selected with:

```json
{
  "syntax_theme": "Solarized Dark",
  "syntax_theme_folder": "themes"
}
```

If the selected theme is missing, the app falls back to the built-in `base16-ocean.dark` theme.
