import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import UnexpectedError from './UnexpectedError';

describe('UnexpectedError', () => {
  it('shows the translated title, and the code only in the opened technical details', () => {
    render(<UnexpectedError error={{ code: 'open_failed' }} />);
    expect(screen.getByRole('heading', { name: 'Não deu para abrir o cartão' })).toBeInTheDocument();
    expect(screen.getByText('open_failed')).not.toBeVisible();
    fireEvent.click(screen.getByText('Detalhes técnicos'));
    expect(screen.getByText('open_failed')).toBeVisible();
  });

  it('an unknown code shows the generic text and the code', () => {
    render(<UnexpectedError error={{ code: 'flux_capacitor' }} />);
    expect(screen.getByRole('heading', { name: 'Algo deu errado' })).toBeInTheDocument();
    fireEvent.click(screen.getByText('Detalhes técnicos'));
    expect(screen.getByText('flux_capacitor')).toBeVisible();
  });
});
