int arr[3] = {1, 2, 3};
int sum(int *a, int n) {
  int s = 0;
  int i;
  for (i = 0; i < n; i++) s += a[i];
  return s;
}
int main(void) {
  return sum(arr, 3);
}
