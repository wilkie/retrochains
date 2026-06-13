int sum_n(int *a, int n) {
  int s = 0;
  while (n--) s += *a++;
  return s;
}
int main(void) {
  int arr[3];
  arr[0] = 10;
  arr[1] = 20;
  arr[2] = 30;
  return sum_n(arr, 3);
}
