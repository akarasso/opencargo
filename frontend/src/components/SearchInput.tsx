interface SearchInputProps {
  value: string;
  placeholder?: string;
  large?: boolean;
  autofocus?: boolean;
  onInput: (value: string) => void;
}

export default function SearchInput(props: SearchInputProps) {
  return (
    <div class="search-input-wrapper">
      <span class="search-input-icon">
        <svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
          <circle cx="11" cy="11" r="8" />
          <line x1="21" y1="21" x2="16.65" y2="16.65" />
        </svg>
      </span>
      <input
        type="text"
        class={`search-input ${props.large ? 'search-input-large' : ''}`}
        placeholder={props.placeholder || 'Search...'}
        value={props.value}
        autofocus={props.autofocus}
        onInput={(e) => props.onInput(e.currentTarget.value)}
      />
    </div>
  );
}
