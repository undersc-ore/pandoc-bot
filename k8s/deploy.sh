sudo k3s kubectl create -f k8s/rabbitmq-service.yml
sudo k3s kubectl create -f k8s/rabbitmq-controller.yml
sudo k3s kubectl create -f k8s/worker.yml
sudo k3s kubectl create -f k8s/deployment.yml
