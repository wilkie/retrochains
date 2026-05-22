int sum_5(int a[5]) {
  return a[0] + a[1] + a[2] + a[3] + a[4];
}
int main(void) {
  int data[5];
  data[0] = 1;
  data[1] = 2;
  data[2] = 3;
  data[3] = 4;
  data[4] = 5;
  return sum_5(data);
}
