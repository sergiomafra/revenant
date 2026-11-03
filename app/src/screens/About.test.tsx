import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import About from './About';

describe('About', () => {
  it('shows the version, the license and the links', () => {
    render(<About onClose={() => {}} />);
    expect(screen.getByText('Versão 0.1.0')).toBeInTheDocument();
    expect(screen.getByText(/GPL-3\.0/)).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Código-fonte' })).toHaveAttribute(
      'href',
      'https://github.com/sergiomafra/revenant',
    );
    expect(screen.getByRole('link', { name: 'Guia de uso' })).toHaveAttribute(
      'href',
      'https://github.com/sergiomafra/revenant#readme',
    );
  });
});
