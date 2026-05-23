int arr[10];

int *pick(int c) {
  return arr + (c ? 1 : 2);
}
