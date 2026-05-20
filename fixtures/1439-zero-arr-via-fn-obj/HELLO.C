int arr[3] = {1, 2, 3};
void zero(int *a, int n) {
  int i;
  for (i = 0; i < n; i++) a[i] = 0;
}
int main(void) {
  zero(arr, 3);
  return arr[1];
}
