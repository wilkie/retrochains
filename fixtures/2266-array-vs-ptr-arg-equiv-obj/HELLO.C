int sum_arr(int a[], int n) {
  int s = 0;
  int i;
  for (i = 0; i < n; i++) s += a[i];
  return s;
}
int sum_ptr(int *p, int n) {
  int s = 0;
  int i;
  for (i = 0; i < n; i++) s += p[i];
  return s;
}
int main(void) {
  static int arr[5] = {1,2,3,4,5};
  return sum_arr(arr, 5) + sum_ptr(arr, 5);
}
