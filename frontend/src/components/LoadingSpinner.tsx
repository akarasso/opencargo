interface LoadingSpinnerProps {
  message?: string;
}

export default function LoadingSpinner(props: LoadingSpinnerProps) {
  return (
    <div class="loading-center">
      <div class="spinner spinner-lg" />
      <span>{props.message || 'Loading...'}</span>
    </div>
  );
}
