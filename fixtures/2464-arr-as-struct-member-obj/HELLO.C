struct Vec {
  int arr[4];
};
int main(void) {
  struct Vec v;
  v.arr[0] = 11;
  v.arr[1] = 22;
  v.arr[2] = 33;
  v.arr[3] = 44;
  return v.arr[0] + v.arr[3];
}
