int data[5] = { 10, 20, 30, 40, 50 };
int sum_arr(int *a, int len) {
  int i;
  int total = 0;
  for (i = 0; i < len; i = i + 1) {
    total = total + a[i];
  }
  return total;
}
int main(void) {
  return sum_arr(data, 5);
}
