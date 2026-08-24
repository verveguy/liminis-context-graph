import { visit } from 'unist-util-visit'

/**
 * Turn ```c4 fenced code blocks into live <C4Playground> islands.
 *
 * The point is a single source of truth that renders usefully in both places:
 *
 *   - **GitHub** shows the fence as a syntax-highlighted code block. Honest and
 *     readable with no build step, which matters because these files are read on
 *     github.com as often as on the docs site.
 *   - **The docs site** replaces it with the interactive component.
 *
 * The alternative was hand-writing `<C4Playground source={...} />` per diagram,
 * which duplicates the source into JSX, makes the page unreadable on GitHub, and
 * forces every page carrying a diagram to be MDX-authored rather than markdown.
 *
 * Fence meta becomes props, so a diagram can say how it wants to be shown:
 *
 *     ```c4 readOnly height=26rem
 *     ```c4 static            (readOnly, drag off — a pure illustration)
 *
 * Unknown meta words are ignored rather than throwing: a fence is content, and
 * a typo in it should not fail a docs build.
 */

const COMPONENT = 'C4Playground'
// An alias, not a relative path. The injected import is the same string for
// every page, but pages need not sit at the same depth — a page in a
// subdirectory is a different number of levels from src/components — so a
// relative path is correct for exactly one depth and silently wrong for the
// rest. Every page here happens to sit at that one depth today, which is
// precisely why this was worth fixing before it bit. Defined in
// astro.config.mjs.
const IMPORT_FROM = '@site/components/C4Playground.tsx'

/** An mdast attribute whose value is a JS expression rather than a string. */
function expressionAttribute(name, value) {
  return {
    type: 'mdxJsxAttribute',
    name,
    value: {
      type: 'mdxJsxAttributeValueExpression',
      value: JSON.stringify(value),
      data: {
        estree: {
          type: 'Program',
          sourceType: 'module',
          body: [
            {
              type: 'ExpressionStatement',
              expression: { type: 'Literal', value, raw: JSON.stringify(value) },
            },
          ],
        },
      },
    },
  }
}

function booleanAttribute(name, value) {
  return value
    ? { type: 'mdxJsxAttribute', name, value: null }
    : expressionAttribute(name, false)
}

function parseMeta(meta) {
  const props = []
  if (!meta) return props
  for (const token of meta.trim().split(/\s+/)) {
    if (!token) continue
    const [key, raw] = token.split('=')
    switch (key) {
      case 'readOnly':
        props.push(booleanAttribute('readOnly', true))
        break
      case 'static':
        // Shorthand: an illustration, not an invitation to edit or drag.
        props.push(booleanAttribute('readOnly', true))
        props.push(booleanAttribute('editable', false))
        break
      case 'editable':
        props.push(booleanAttribute('editable', raw !== 'false'))
        break
      case 'height':
        if (raw) props.push({ type: 'mdxJsxAttribute', name: 'height', value: raw })
        break
      default:
        // Ignored on purpose — see the note above.
        break
    }
  }
  return props
}

/** `import C4Playground from '…'`, built as estree rather than parsed. */
function importNode() {
  return {
    type: 'mdxjsEsm',
    value: `import ${COMPONENT} from '${IMPORT_FROM}'`,
    data: {
      estree: {
        type: 'Program',
        sourceType: 'module',
        body: [
          {
            type: 'ImportDeclaration',
            specifiers: [
              {
                type: 'ImportDefaultSpecifier',
                local: { type: 'Identifier', name: COMPONENT },
              },
            ],
            source: { type: 'Literal', value: IMPORT_FROM, raw: `'${IMPORT_FROM}'` },
            attributes: [],
          },
        ],
      },
    },
  }
}

/**
 * The generated `<picture>` blocks exist for GitHub, which has no build step.
 * Here the island renders the same diagram interactively, so showing both would
 * be duplication — they are stripped. See scripts/render-diagrams.mjs.
 *
 * Identified by the paths inside them rather than by a marker comment: MDX does
 * not permit HTML comments at all, so `<!-- … -->` is a syntax error rather than
 * a marker.
 *
 * Bare `<img>` is still matched: a page written before the light/dark
 * `<picture>` existed should lose its old block rather than keep it beside the
 * island.
 */
function referencesGeneratedDiagram(node) {
  if (node.type !== 'mdxJsxFlowElement' && node.type !== 'mdxJsxTextElement') return false
  return (node.attributes ?? []).some(
    (a) =>
      (a.name === 'src' || a.name === 'srcset') &&
      typeof a.value === 'string' &&
      a.value.includes('/diagrams/'),
  )
}

function isGeneratedImage(node) {
  if (node.type !== 'mdxJsxFlowElement' && node.type !== 'mdxJsxTextElement') return false
  if (node.name === 'img') return referencesGeneratedDiagram(node)
  if (node.name !== 'picture') return false
  return (node.children ?? []).some(referencesGeneratedDiagram)
}

function stripRenderedImages(tree) {
  const doomed = []
  visit(tree, (node, index, parent) => {
    if (parent && isGeneratedImage(node)) doomed.push({ index, parent })
  })
  // Remove back-to-front so earlier indices stay valid.
  for (const { index, parent } of doomed.reverse()) parent.children.splice(index, 1)
}

export function remarkC4() {
  return (tree) => {
    stripRenderedImages(tree)

    const replacements = []

    visit(tree, 'code', (node, index, parent) => {
      if (node.lang !== 'c4' || !parent) return
      replacements.push({ node, index, parent })
    })

    if (replacements.length === 0) return

    for (const { node, index, parent } of replacements) {
      parent.children[index] = {
        type: 'mdxJsxFlowElement',
        name: COMPONENT,
        attributes: [
          // The drag layer measures the live SVG via getScreenCTM, which does
          // not exist during a server render, so these cannot be hydrated with
          // client:visible.
          { type: 'mdxJsxAttribute', name: 'client:only', value: 'react' },
          expressionAttribute('source', node.value),
          ...parseMeta(node.meta),
        ],
        children: [],
      }
    }

    // One import for the file, regardless of how many diagrams it holds.
    tree.children.unshift(importNode())
  }
}

export default remarkC4
