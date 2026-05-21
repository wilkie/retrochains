int sum_arr(int *a, int n) {
  int total = 0;
  int i;
  for (i = 0; i < n; i++) total += a[i];
  return total;
}
int main(void) {
  int arr[3];
  arr[0] = 10; arr[1] = 20; arr[2] = 30;
  return sum_arr(arr, 3);
}
