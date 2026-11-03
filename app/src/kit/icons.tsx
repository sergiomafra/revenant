// Lucide icons that lucide-react 1.x no longer ships, drawn with Lucide's own geometry (ISC License,
// https://lucide.dev), the same paths as docs/superpowers/mockups/m2/kit.js. Same props as a lucide-react icon.
import type { SVGProps } from 'react';

type Props = SVGProps<SVGSVGElement>;

function icon(paths: string[], name: string) {
  function Icon({ className, ...rest }: Props) {
    return (
      <svg
        xmlns="http://www.w3.org/2000/svg"
        width="24"
        height="24"
        viewBox="0 0 24 24"
        fill="none"
        stroke="currentColor"
        strokeWidth="2"
        strokeLinecap="round"
        strokeLinejoin="round"
        className={['lucide', `lucide-${name}`, className].filter(Boolean).join(' ')}
        {...rest}
      >
        {paths.map((d) => (
          <path key={d} d={d} />
        ))}
      </svg>
    );
  }
  return Icon;
}

/** Lucide "sd-card". */
export const SdCard = icon(
  ['M10 2v4', 'M14 2v4', 'M18 22H6a2 2 0 0 1-2-2V6l4-4h10a2 2 0 0 1 2 2v16a2 2 0 0 1-2 2z'],
  'sd-card',
);
