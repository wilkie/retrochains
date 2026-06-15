int sum(int a[], int n) {
  int i;
  int total;
  total = 0;
  for (i = 0; i < n; i = i + 1) {
    total = total + a[i];
  }
  return total;
}
int main(void) {
  int data[4];
  data[0] = 10;
  data[1] = 20;
  data[2] = 30;
  data[3] = 40;
  return sum(data, 4);
}
