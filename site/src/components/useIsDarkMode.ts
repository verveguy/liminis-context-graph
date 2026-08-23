import { useEffect, useState } from 'react'

/**
 * Track the site's current theme.
 *
 * `C4InteractiveRenderer` takes `isDarkMode` as a prop rather than reading the
 * document itself — deliberately, since the library has no idea how its host
 * decides what "dark" means. That makes keeping it current the host's job, and
 * a host that reads the theme once at mount has a diagram that keeps last
 * night's colours until something else forces a re-render. Clicking one used to
 * be what did it here, which is not a feature.
 *
 * Two sources have to be watched, because Starlight has three states:
 *
 *   - An explicit choice sets `data-theme="dark"|"light"` on `<html>`, so the
 *     attribute is observed.
 *   - The default "auto" sets nothing, and the theme follows the OS. So
 *     `prefers-color-scheme` is watched too — otherwise a diagram would not
 *     follow the system flipping to dark at sunset.
 */
export function useIsDarkMode(): boolean {
  const [isDark, setIsDark] = useState(false)

  useEffect(() => {
    const read = () => {
      const explicit = document.documentElement.dataset.theme
      if (explicit === 'dark') return true
      if (explicit === 'light') return false
      return window.matchMedia('(prefers-color-scheme: dark)').matches
    }

    setIsDark(read())

    const observer = new MutationObserver(() => setIsDark(read()))
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ['data-theme'],
    })

    const media = window.matchMedia('(prefers-color-scheme: dark)')
    const onMedia = () => setIsDark(read())
    media.addEventListener('change', onMedia)

    return () => {
      observer.disconnect()
      media.removeEventListener('change', onMedia)
    }
  }, [])

  return isDark
}
