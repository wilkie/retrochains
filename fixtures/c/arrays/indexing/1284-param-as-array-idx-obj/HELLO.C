int arr[5];
int get(int i) {
  return arr[i];
}
int main(void) {
  arr[0] = 10;
  arr[1] = 20;
  arr[2] = 30;
  return get(1);
}
