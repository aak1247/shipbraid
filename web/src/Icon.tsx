import type { ReactNode, SVGProps } from "react";

type IconName =
  | "activity"
  | "arrow"
  | "check"
  | "chevron"
  | "clock"
  | "copy"
  | "layers"
  | "link"
  | "lock"
  | "menu"
  | "pause"
  | "follow"
  | "wrap"
  | "refresh"
  | "search"
  | "shield"
  | "terminal"
  | "x";

interface IconProps extends SVGProps<SVGSVGElement> {
  name: IconName;
  size?: number;
}

const paths: Record<IconName, ReactNode> = {
  activity: <><path d="M3 10h3l2-5 4 10 2-5h3" /><path d="M3 16h14" /></>,
  arrow: <><path d="M4 10h11" /><path d="m11 5 5 5-5 5" /></>,
  check: <path d="m4 10 4 4 8-8" />,
  chevron: <path d="m5 7 5 5 5-5" />,
  clock: <><circle cx="10" cy="10" r="7" /><path d="M10 6v4l3 2" /></>,
  copy: <><rect x="7" y="7" width="9" height="10" rx="1.5" /><path d="M13 4H5a1 1 0 0 0-1 1v9" /></>,
  layers: <><path d="m10 3 7 4-7 4-7-4 7-4Z" /><path d="m3 11 7 4 7-4M3 15l7 4 7-4" /></>,
  link: <><path d="M8 12a4 4 0 0 0 5.7 0l2-2A4 4 0 0 0 10 4.3l-1.1 1.1" /><path d="M12 8a4 4 0 0 0-5.7 0l-2 2A4 4 0 0 0 10 15.7l1.1-1.1" /></>,
  lock: <><rect x="4" y="9" width="12" height="9" rx="1.5" /><path d="M7 9V6a3 3 0 0 1 6 0v3M10 12v3" /></>,
  menu: <><path d="M3 5h14M3 10h14M3 15h14" /></>,
  pause: <><path d="M7 4v12M13 4v12" /></>,
  follow: <><path d="M10 3v10m-4-4 4 4 4-4M4 17h12" /></>,
  wrap: <><path d="M3 5h14M3 10h11a3 3 0 0 1 0 6h-4m2-2-2 2 2 2M3 15h3" /></>,
  refresh: <><path d="M16 7V3l-2 2a6 6 0 0 0-10 4" /><path d="M4 13v4l2-2a6 6 0 0 0 10-4" /></>,
  search: <><circle cx="8.5" cy="8.5" r="5.5" /><path d="m13 13 4 4" /></>,
  shield: <><path d="M10 2.8 16 5v4.4c0 3.7-2.5 6.2-6 7.8-3.5-1.6-6-4.1-6-7.8V5l6-2.2Z" /><path d="m7.5 9.8 1.7 1.7 3.4-3.6" /></>,
  terminal: <><path d="m4 6 4 4-4 4M10 14h6" /></>,
  x: <><path d="m5 5 10 10M15 5 5 15" /></>,
};

export function Icon({ name, size = 16, ...props }: IconProps) {
  return (
    <svg
      aria-hidden="true"
      width={size}
      height={size}
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.55"
      strokeLinecap="round"
      strokeLinejoin="round"
      focusable="false"
      {...props}
    >
      {paths[name]}
    </svg>
  );
}
