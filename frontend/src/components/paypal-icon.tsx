/**
 * PayPal's monogram (lucide-react in this build ships no brand icons).
 *
 * Three paths, because the mark is two overlapping P's and the region where
 * they cross is its own colour. They are painted back, front, overlap — the
 * shapes deliberately overlap rather than abut, so no hairline seam can open
 * between them at a fractional device pixel.
 *
 * Unlike {@link ChargebeeIcon} this cannot use `currentColor`: a two-tone mark
 * has no single colour to inherit. The fills come from `--brand-paypal-*`
 * instead, which is the same "deliberately not our palette" statement one layer
 * down. Both P colours change with the theme; see `index.css` for why.
 *
 * Traced from the 256px monogram PNG in PayPal's press assets, so the curves
 * are a close fit rather than their exact published Bézier control points. If
 * an official SVG turns up, swap these three `d` strings for it — nothing else
 * here has to change.
 */
export function PaypalIcon({ className }: { className?: string }) {
  return (
    <svg viewBox="0 0 163 197" className={className} aria-hidden="true">
      <path
        className="fill-(--brand-paypal-back) dark:fill-(--brand-paypal-back-on-dark)"
        d="M24 0C33.3-0.1 42.6-0.1 51.9-0.2C56.2-0.2 60.5-0.2 64.8-0.2C69-0.3 73.2-0.3 77.4-0.3C78.9-0.3 80.5-0.3 82.1-0.3C98.5-0.5 114.4 1.6 126.9 13.3C135.1 22.4 140 33.7 140 46C139.1 58.7 134.5 69.5 126 79C125.4 79.8 124.8 80.5 124.1 81.3C114.5 92 100.1 97.7 86 99C83.1 99.1 80.2 99.1 77.3 99.1C76.6 99.1 75.8 99.1 75 99.1C72.5 99.1 70 99.1 67.5 99.1C65.8 99.1 64.1 99.1 62.4 99C58.3 99 54.1 99 50 99C49.9 100.2 49.7 101.3 49.6 102.5C47.1 121.1 44 139.5 41 158C27.5 158 13.9 158 0 158C1.1 145.6 1.1 145.6 1.9 140.3C2.1 139.1 2.3 137.9 2.5 136.7C2.7 135.4 2.9 134.1 3.1 132.8C3.3 131.4 3.5 130 3.8 128.6C4.3 124.9 4.9 121.1 5.5 117.4C6.1 113.4 6.7 109.5 7.3 105.6C8.4 99 9.4 92.4 10.4 85.8C11.7 77.3 13.1 68.8 14.4 60.4C15.5 53.1 16.6 45.8 17.8 38.5C18.1 36.2 18.5 33.8 18.9 31.5C19.4 27.8 20 24.1 20.6 20.5C20.9 18.5 21.2 16.5 21.5 14.5C21.7 13.3 21.9 12.1 22.1 10.9C22.2 9.9 22.4 8.9 22.5 7.8C23 5.2 23.5 2.6 24 0Z"
      />
      <path className="fill-(--brand-paypal-front) dark:fill-(--brand-paypal-front-on-dark)" d="M59 39C66.5 39 74 38.9 81.5 38.9C84.9 38.9 88.4 38.9 91.9 38.8C95.9 38.8 99.9 38.8 103.9 38.8C105.2 38.8 106.4 38.8 107.6 38.8C124.5 38.8 139.6 41.8 152 54C161.3 64.8 164.2 76 163.2 89.9C161.7 104 153.9 117.1 143 126C131.7 134 121.5 138.2 107.6 138.1C107.3 138.1 107.3 138.1 105.6 138.1C103.4 138.1 101.3 138.1 99.1 138.1C97.7 138.1 96.2 138.1 94.7 138C91.2 138 87.6 138 84 138C83.9 138.5 83.9 138.5 83.7 140.9C82.4 153.1 80.4 165 78.4 177.1C77.3 183.6 76.1 190.2 75 197C61.5 197 47.9 197 34 197C36.7 177.8 39.7 158.7 42.8 139.6C43.9 132.3 45.1 124.9 46.2 117.6C46.5 115.8 46.8 114 47.1 112.2C49 100.6 50.8 89 52.6 77.4C52.8 76.2 53 75 53.2 73.8C54 68.3 54.9 62.8 55.7 57.3C56 55.4 56.3 53.6 56.6 51.7C56.7 50.8 56.9 50 57 49.1C57.5 45.7 58.1 42.4 59 39Z" />
      <path className="fill-(--brand-paypal-overlap) dark:fill-(--brand-paypal-overlap-on-dark)" d="M59 39C66.5 39 74 38.9 81.5 38.9C84.9 38.9 88.4 38.9 91.9 38.8C95.9 38.8 99.9 38.8 103.9 38.8C105.2 38.8 106.4 38.8 107.6 38.8C117.2 38.8 126.9 39.2 135.8 43.1C136.6 43.5 137.4 43.8 138.2 44.2C138.5 44.3 138.5 44.3 140 45C138.6 58.3 135 68.9 126 79C125.7 79.4 125.7 79.4 124.1 81.3C114.5 92 100.1 97.7 86 99C83.1 99.1 80.2 99.1 77.3 99.1C76.6 99.1 75.8 99.1 75 99.1C72.5 99.1 70 99.1 67.5 99.1C65.8 99.1 64.1 99.1 62.4 99C58.3 99 54.1 99 50 99C50.6 91.7 51.4 84.5 52.6 77.2C52.7 76.2 52.9 75.3 53.1 74.3C53.4 72.2 53.7 70.2 54 68.2C54.5 65 55 61.9 55.5 58.8C55.8 56.8 56.1 54.8 56.4 52.8C56.6 51.9 56.7 51 56.8 50C57 49.2 57.1 48.3 57.3 47.4C57.4 46.7 57.5 45.9 57.6 45.1C58 43 58.5 41 59 39Z" />
    </svg>
  );
}
