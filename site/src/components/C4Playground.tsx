/**
 * The playground, wired to this site's theme.
 *
 * The component itself lives in the package now
 * (`@liminis/diagrams/playground`); what remains here is the part that is
 * genuinely this site's: which theme it is in. `useIsDarkMode` is exported
 * alongside the component for hosts following the `data-theme` convention,
 * which Starlight does.
 *
 * The remark plugin imports this file rather than the package directly, so that
 * every fence on the site gets the theme wiring without repeating it.
 */
import { C4Playground, useIsDarkMode } from '@liminis/diagrams/playground'
import type { C4PlaygroundProps } from '@liminis/diagrams/playground'

export default function ThemedC4Playground(props: Omit<C4PlaygroundProps, 'isDarkMode'>) {
  return <C4Playground {...props} isDarkMode={useIsDarkMode()} />
}
